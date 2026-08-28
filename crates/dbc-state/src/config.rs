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
    ///
    /// INERT IN WORKSPACE MODE (design §W8): while a workspace is active
    /// the scripts root is always `<workspace>/scripts`, so a hand-edited
    /// `scripts_dir` in a workspace `config.toml` is ignored — one root per
    /// mode, no precedence question — and the app never WRITES this field
    /// there either. The seam is `dbc-ui`'s `scripts_root_for`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_dir: Option<String>,
}

/// FINAL-REVIEW MAJOR-2 — proof that overwriting `config.toml` will not
/// destroy anything, demanded BY TYPE by [`AppConfig::save`].
///
/// The invariant is `dbc-ui`'s `guard_corrupt_config`: a `config.toml`
/// that failed to parse holds the user's entire connection list in a form
/// the app could not read, and saving defaults over it destroys data that
/// was still recoverable by hand. That rule was enforced by a text audit
/// keyed on the literal `.config.save(` — and a reviewer walked around it
/// by rebinding the receiver (`let cfg = &self.config; cfg.save(…)`) with
/// zero warnings and every audit green.
///
/// A witness whose constructor is merely private to `dbc-state` would not
/// help: `dbc-ui` could not mint it, and any `pub fn new()` we added would
/// be mintable by exactly the code the rail is aimed at. So this witness
/// is not proof that a FUNCTION was called — it is proof of the
/// PRECONDITION, established here, on disk, by [`AppConfig::verify_savable`].
/// There is no other constructor, in this crate or any other, and the
/// check it performs is the real one rather than a token gesture.
///
/// A borrow (`&ConfigSaveGuard`) is enough: the guard says something about
/// the FILE, not about a one-shot permission, and a caller that saves the
/// same path twice in a row has not lied. Contrast `dbc-ui`'s
/// `SaveAllowed`, which is consumed because it is about a MOMENT.
///
/// RE-VERIFY NIT-1: the guard NAMES ITS PATH, and [`AppConfig::save`]
/// refuses a guard minted for a different one. Without that, the type
/// proved less than this doc claimed — `verify_savable(a)` followed by
/// `save(b, &g)` type-checked cleanly, so „you proved the file you are
/// about to overwrite parses" was really only „you proved SOME file
/// parses". All six live call sites were already correct; the point is
/// that the compiler now agrees with the sentence.
#[derive(Debug)]
pub struct ConfigSaveGuard(std::path::PathBuf);

/// What [`AppConfig::verify_config`] found. The two failure arms are
/// deliberately NOT one (re-verify NIT-2): only `Unparsable` means the
/// bytes on disk are unusable, and only `Unparsable` may be moved aside.
#[derive(Debug)]
pub enum ConfigVerdict {
    /// Absent (a first save destroys nothing) or it parses.
    Savable(ConfigSaveGuard),
    /// Present but could not be READ right now — locked, permissions, a
    /// share that blinked. Says nothing about the content, so the caller
    /// must refuse rather than rescue.
    Unreadable(StateError),
    /// Present, readable, and NOT valid TOML. The case the corrupt-config
    /// guard exists for.
    Unparsable(StateError),
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<AppConfig, StateError> {
        if !path.exists() { return Ok(AppConfig::default()); }
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// THE only mint of [`ConfigSaveGuard`]. Succeeds when the file at
    /// `path` is absent (a first save destroys nothing) or parses (an
    /// overwrite loses nothing the app could not already read).
    ///
    /// Deliberately re-reads rather than trusting a flag captured at
    /// startup: this crate has no way to know how old such a flag is, and
    /// this phase has paid five times for checks that describe the past.
    /// `config.toml` is a handful of kilobytes and a save is a user
    /// gesture, so the read costs nothing anyone can perceive.
    pub fn verify_savable(path: &Path) -> Result<ConfigSaveGuard, StateError> {
        match AppConfig::verify_config(path) {
            ConfigVerdict::Savable(g) => Ok(g),
            ConfigVerdict::Unreadable(e) | ConfigVerdict::Unparsable(e) => Err(e),
        }
    }

    /// [`verify_savable`](AppConfig::verify_savable) with the REASON kept
    /// apart, in ONE read so the two cannot race each other.
    ///
    /// RE-VERIFY NIT-2. `dbc-ui`'s `guard_corrupt_config` treated any
    /// refusal as corruption and renamed `config.toml` to
    /// `.corrupt-bak` — so a file that was merely unreadable for a moment
    /// (locked by an editor, an antivirus scan, a network share
    /// hiccupping) got moved aside and replaced, even though it was
    /// perfectly good. Nothing is destroyed, but it manufactures
    /// `.corrupt-bak` files out of transient conditions and tells the user
    /// their config was corrupt when it was not.
    ///
    /// „Absent" is [`ConfigVerdict::Savable`], matching
    /// [`AppConfig::load`]: a first save destroys nothing.
    pub fn verify_config(path: &Path) -> ConfigVerdict {
        if !path.exists() {
            return ConfigVerdict::Savable(ConfigSaveGuard(path.to_path_buf()));
        }
        match std::fs::read_to_string(path) {
            Err(e) => ConfigVerdict::Unreadable(e.into()),
            Ok(text) => match toml::from_str::<AppConfig>(&text) {
                Ok(_) => ConfigVerdict::Savable(ConfigSaveGuard(path.to_path_buf())),
                Err(e) => ConfigVerdict::Unparsable(e.into()),
            },
        }
    }

    /// `guard` is unused at runtime and load-bearing at compile time: it
    /// cannot be obtained except from [`AppConfig::verify_savable`], so no
    /// call syntax — receiver rebinding, UFCS, a macro — reaches this
    /// writer without the corrupt-config question having been asked and
    /// answered against the actual file.
    pub fn save(&self, path: &Path, guard: &ConfigSaveGuard) -> Result<(), StateError> {
        // RE-VERIFY NIT-1: the guard is proof about ONE file.
        //
        // The compare is EXACT BYTES, deliberately, and re-verify's own NIT
        // is right that this is a second path-comparison convention beside
        // `dbc-ui`'s `same_path_ci` / `fsutil::fold_name`. It is not a
        // second rail for the same job, though: `same_path_ci` answers
        // „do these two names reach the same file on disk", which is a
        // question about the FILESYSTEM and needs the Unicode fold. This
        // asks „is this the same `PathBuf` value the caller proved
        // something about", which is a question about ONE CALLER'S OWN
        // BOOKKEEPING — every live site passes the very same
        // `&self.config_path` expression to both calls. Folding here would
        // make the check LOOSER (two spellings of one file would pass) for
        // a caller that has no business using two spellings, and would
        // drag `dbc-state` into owning a case-fold policy it currently
        // does not. Fail-safe either way: a mismatch refuses.
        if guard.0 != path {
            return Err(StateError {
                message: format!(
                    "interní chyba: potvrzení o config.toml patří jinému souboru ({})",
                    guard.0.display()
                ),
            });
        }
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
    crate::workspace::profile_dir().join("config.toml")
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
        sample().save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded, sample());
    }

    /// FINAL-REVIEW MAJOR-2: the rail behind `dbc-ui`'s
    /// `guard_corrupt_config`, now enforced by the type system rather than
    /// by a text audit keyed on one spelling of the receiver.
    ///
    /// `save` cannot be called without a `ConfigSaveGuard`, and there is
    /// exactly one way to obtain one: prove the file on disk is absent or
    /// parses. So „defaults written over a config.toml the app could not
    /// read" is not merely audited against — it does not compile, and if
    /// someone routes around the audit some sixth way, the write still
    /// cannot happen.
    #[test]
    fn the_save_witness_can_only_be_minted_over_an_intact_or_absent_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");

        // Absent: a first save destroys nothing.
        assert!(AppConfig::verify_savable(&p).is_ok());
        sample().save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
        // Intact: an overwrite loses nothing the app could not read.
        assert!(AppConfig::verify_savable(&p).is_ok());

        // Corrupt — the case the whole rail exists for. This is the user's
        // entire connection list in a form only a human can rescue.
        std::fs::write(&p, b"connections = [ this is not toml").unwrap();
        assert!(
            AppConfig::verify_savable(&p).is_err(),
            "an unparsable config.toml must not yield a save witness"
        );
        // And the corrupt bytes are still there: refusing to mint is not a
        // side-effecting operation.
        assert!(std::fs::read_to_string(&p).unwrap().contains("this is not toml"));
    }

    /// RE-VERIFY NIT-1: the guard is proof about ONE file, and `save`
    /// enforces that. Before this, `verify_savable(a)` + `save(b, &g)`
    /// type-checked — so the doc's promise („you proved the file you are
    /// about to overwrite parses") was really only „you proved some file
    /// parses".
    #[test]
    fn a_guard_minted_for_one_path_cannot_authorise_a_write_to_another() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("config.toml");
        let b = dir.path().join("jiny.toml");
        let guard = AppConfig::verify_savable(&a).unwrap();
        let err = sample().save(&b, &guard).unwrap_err();
        assert!(err.message.contains("jinému souboru"), "{}", err.message);
        assert!(!b.exists(), "the refused write must not have happened");
        // …and the guard still works for the path it was minted for.
        sample().save(&a, &guard).unwrap();
        assert!(a.is_file());
    }

    /// RE-VERIFY NIT-2: „I could not read it" and „it is not TOML" are
    /// different facts, and only the second licenses moving the file
    /// aside. Collapsing them made `dbc-ui` rename a perfectly good
    /// `config.toml` to `.corrupt-bak` whenever a read failed for a
    /// moment — a lock, an antivirus scan, a share blinking.
    #[test]
    fn the_verdict_tells_unreadable_apart_from_unparsable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        // Absent — a first save destroys nothing.
        assert!(matches!(AppConfig::verify_config(&p), ConfigVerdict::Savable(_)));
        // Present and valid.
        sample().save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
        assert!(matches!(AppConfig::verify_config(&p), ConfigVerdict::Savable(_)));
        // Present and not TOML — the only arm that may be rescued.
        std::fs::write(&p, b"connections = [ this is not toml").unwrap();
        assert!(matches!(AppConfig::verify_config(&p), ConfigVerdict::Unparsable(_)));
        // Present but unreadable. A DIRECTORY at the path is the portable
        // way to make `read_to_string` fail while `exists()` is true.
        let d = dir.path().join("as_dir.toml");
        std::fs::create_dir(&d).unwrap();
        assert!(
            matches!(AppConfig::verify_config(&d), ConfigVerdict::Unreadable(_)),
            "a read failure must not be reported as corruption"
        );
        // The convenience wrapper still collapses both into `Err`, which
        // is all its callers need.
        assert!(AppConfig::verify_savable(&d).is_err());
        assert!(AppConfig::verify_savable(&p).is_err());
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
        sample().save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded.is_favourite(&fav), true);

        // Toggle off
        let mut config2 = loaded;
        let state = config2.toggle_favourite(fav.clone());
        assert_eq!(state, false);
        assert_eq!(config2.is_favourite(&fav), false);

        // Save and load again
        config2.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        sample_with_tools().save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        config.save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
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
        sample().save(&p, &AppConfig::verify_savable(&p).unwrap()).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("mssql"));
    }
}
