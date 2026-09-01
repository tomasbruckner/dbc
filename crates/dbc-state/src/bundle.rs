//! The portable settings bundle — one file that carries a machine's
//! connections, and the passwords to them, to another machine.
//!
//! ## Why this is a format and not just instructions
//!
//! The files it carries were always portable on their own. The vault is an
//! Argon2id + ChaCha20-Poly1305 envelope with NO binding to the machine
//! that wrote it — no DPAPI, no hostname, no hardware id — so copying
//! `vault.bin` to another computer and typing the same master password has
//! always worked. „Copy three files" was the true answer before this module
//! existed, and this module adds no capability.
//!
//! What it adds is that the three files travel TOGETHER, and that the
//! machine-specific parts do not travel at all.
//!
//! The vault keys its secrets by CONNECTION ID (`conn-18ce94b370664078`),
//! and `views.toml` keys its column widths by that same id. So a vault
//! without its `config.toml` is a set of passwords with nothing to attach
//! them to, and a `config.toml` without its vault is a list of connections
//! that every one of them asks for a password nobody wrote down. Copying by
//! hand gets that pairing wrong exactly once — on the new machine, where
//! the old one is no longer in front of you to try again.
//!
//! ## What is deliberately NOT in it
//!
//! * `history.sqlite` — the SQL you typed, including any literal you put in
//!   a `WHERE`. Machine-local by design (§W5), and not a setting.
//! * `params.toml` — last-used `:param` values: same reason, arbitrary
//!   user-typed literals.
//! * `dbc.log`, `schema-cache/`, `sessions/`, `server-versions.json` —
//!   diagnostic or derived. They
//!   would only arrive stale.
//! * `tool_paths` and `scripts_dir` INSIDE `config.toml` — absolute paths to
//!   THIS machine's `psql`/`sqlcmd` and to a local folder. They are the one
//!   part of the config that describes the machine rather than the work, so
//!   [`build`] strips them on the way out rather than importing paths that
//!   point at nothing. `machine_local_paths_are_stripped` is the rail.
//!
//! ## What it does not do to the secrets
//!
//! Nothing at all. The vault goes in as the ciphertext it already is on
//! disk and comes out the same bytes. There is no second encryption layer
//! and no export passphrase: the bundle is exactly as strong as the master
//! password, on the source machine and the target alike. Two consequences
//! worth stating out loud, because both surprise people:
//!
//! * **Export needs no master password**, since nothing is ever decrypted.
//! * **The imported vault opens with the SOURCE machine's master
//!   password**, not the target's. Importing does not merge two vaults; it
//!   replaces one with the other.
//!
//! `bundle_never_contains_a_plaintext_secret` is the rail on the first
//! claim, and it is deliberately written so it cannot pass vacuously.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, StateError};
use crate::workspace::Paths;

/// Stamped into every bundle so a file that is merely JSON is refused with
/// a sentence about what it is, rather than with a serde error about a
/// missing field.
pub const FORMAT: &str = "dbc-bundle";

/// Bumped only for a change a `VERSION`-aware reader could not survive.
/// [`parse`] refuses anything HIGHER (it cannot know what it would be
/// dropping) and accepts anything lower, which is what `#[serde(default)]`
/// on the optional members is for.
pub const VERSION: u32 = 1;

/// Extension suggested by both file dialogs. Not enforced on read — a file
/// is identified by [`FORMAT`] inside it, never by its name.
pub const EXT: &str = "dbcx";

fn err(m: impl Into<String>) -> StateError {
    StateError { message: m.into() }
}

/// The whole file. Plain JSON rather than a zip so it stays inspectable:
/// you can open a bundle in an editor and see for yourself that the vault
/// member is ciphertext and that no password of yours is in there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    /// Unix seconds, informational only — nothing branches on it. Seconds
    /// rather than a formatted timestamp because `dbc-state` has no time
    /// crate and a wrong-timezone string is worse than a number.
    #[serde(default)]
    pub created_unix: u64,
    /// The app version that wrote it, for a support conversation.
    #[serde(default)]
    pub app_version: String,
    /// `config.toml`, already stripped of the machine-local members.
    pub config_toml: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub views_toml: Option<String>,
    /// The sealed vault envelope, byte for byte as it sits on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_bin: Option<String>,
}

/// What a bundle says about itself, WITHOUT unsealing anything.
///
/// This is what the import dialog shows before the user agrees to replace
/// their current settings — so it must be answerable from the ciphertext.
/// Note what is missing: the number of stored passwords. Counting them
/// would mean decrypting the vault, which needs the source machine's
/// master password, which the person importing may not have typed yet.
/// Claiming a count we cannot check would be worse than `has_vault`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub connections: Vec<String>,
    pub has_vault: bool,
    pub has_views: bool,
    pub created_unix: u64,
    pub app_version: String,
}

fn read_optional(path: &Path) -> Result<Option<String>, StateError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(format!("{} nejde přečíst: {e}", path.display()))),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Collect the exportable state of one context.
///
/// Refuses rather than exports a degraded file in two cases, both of which
/// are „you are about to carry the wrong thing to a machine where you
/// cannot check":
///
/// * an unreadable `config.toml` — exporting `AppConfig::default()` here
///   would produce a valid, empty, entirely wrong bundle. This is the same
///   danger `ConfigSaveGuard` exists for, met from the other direction.
/// * a `config.toml` with no connections at all — the only thing a bundle
///   is FOR. In practice this means the wrong profile or workspace is
///   active, and finding that out now beats finding it out later.
pub fn build(paths: &Paths, app_version: &str) -> Result<Bundle, StateError> {
    let mut config = AppConfig::load(&paths.config).map_err(|e| {
        err(format!(
            "nastavení ({}) nejde přečíst, takže není co vyvézt: {}",
            paths.config.display(),
            e.message
        ))
    })?;
    if config.connections.is_empty() {
        return Err(err(format!(
            "v {} nejsou žádná uložená připojení — zkontroluj, jestli je aktivní ten profil, \
             který jsi čekal",
            paths.config.display()
        )));
    }
    // The machine-local members, dropped here rather than on import: a
    // bundle should be true wherever it is opened, and a path to
    // `C:\Users\…\psql.exe` is not true on the machine it is going to.
    config.tool_paths = Default::default();
    config.scripts_dir = None;

    let config_toml = toml::to_string_pretty(&config)
        .map_err(|e| err(format!("nastavení nejde zapsat do balíčku: {e}")))?;

    let vault_bin = match read_optional(&paths.vault)? {
        Some(text) => {
            // Belt on the one claim this whole module makes about secrets.
            // If the file at the vault's path is NOT the sealed envelope —
            // a leftover, a hand-edit, a restore of the wrong thing — then
            // copying it into a bundle could carry readable text out of the
            // machine, which is the single outcome that must not happen
            // quietly.
            if !crate::vault::text_is_sealed_envelope(&text) {
                return Err(err(format!(
                    "soubor trezoru ({}) nemá tvar zašifrované obálky — export zastaven, \
                     aby se nevyvezlo něco čitelného",
                    paths.vault.display()
                )));
            }
            Some(text)
        }
        None => None,
    };

    Ok(Bundle {
        format: FORMAT.to_string(),
        version: VERSION,
        created_unix: now_unix(),
        app_version: app_version.to_string(),
        config_toml,
        views_toml: read_optional(&paths.views)?,
        vault_bin,
    })
}

pub fn to_json(bundle: &Bundle) -> Result<String, StateError> {
    serde_json::to_string_pretty(bundle).map_err(|e| err(format!("balíček nejde zapsat: {e}")))
}

/// Read a bundle and validate it COMPLETELY.
///
/// Everything that could make [`apply`] fail half-way is checked here, on
/// text, before any caller has been told the file is usable — the config
/// member really parses as an `AppConfig`, the vault member really is a
/// sealed envelope. `apply` overwrites files, so „refuse late" would mean
/// „refuse after destroying something".
pub fn parse(text: &str) -> Result<Bundle, StateError> {
    let bundle: Bundle = serde_json::from_str(text)
        .map_err(|_| err("tohle není soubor s nastavením dbc"))?;
    if bundle.format != FORMAT {
        return Err(err(format!(
            "cizí formát „{}“ — čekal jsem „{FORMAT}“",
            bundle.format
        )));
    }
    if bundle.version > VERSION {
        return Err(err(format!(
            "balíček je z novější verze aplikace (formát {}, tahle umí {VERSION}) — aktualizuj dbc",
            bundle.version
        )));
    }
    toml::from_str::<AppConfig>(&bundle.config_toml)
        .map_err(|e| err(format!("nastavení uvnitř balíčku nejde přečíst: {e}")))?;
    if let Some(v) = &bundle.vault_bin {
        if !crate::vault::text_is_sealed_envelope(v) {
            return Err(err(
                "trezor uvnitř balíčku nemá tvar zašifrované obálky — balíček odmítnut",
            ));
        }
    }
    Ok(bundle)
}

/// Answer the import dialog's questions from the ciphertext alone.
///
/// Infallible on a bundle that came from [`parse`], which already proved
/// the config member parses; the `Result` is for callers that built a
/// `Bundle` some other way.
pub fn summary(bundle: &Bundle) -> Result<Summary, StateError> {
    let config: AppConfig = toml::from_str(&bundle.config_toml)
        .map_err(|e| err(format!("nastavení uvnitř balíčku nejde přečíst: {e}")))?;
    Ok(Summary {
        connections: config.connections.into_iter().map(|c| c.name).collect(),
        has_vault: bundle.vault_bin.is_some(),
        has_views: bundle.views_toml.is_some(),
        created_unix: bundle.created_unix,
        app_version: bundle.app_version.clone(),
    })
}

pub fn write(bundle: &Bundle, path: &Path) -> Result<(), StateError> {
    let text = to_json(bundle)?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = crate::fsutil::tmp_path_for(path);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read(path: &Path) -> Result<Bundle, StateError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| err(format!("{} nejde přečíst: {e}", path.display())))?;
    parse(&text)
}

/// What [`apply`] did, so the caller can tell the user where their old
/// settings went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub written: Vec<PathBuf>,
    /// `(original path, where the previous content was moved)`.
    pub backed_up: Vec<(PathBuf, PathBuf)>,
}

/// Where a replaced file is parked. Timestamped rather than a fixed
/// `.bak`, so a second import cannot quietly destroy the copy the first one
/// made — which would defeat the entire point of making one.
fn backup_path(target: &Path, stamp: u64) -> PathBuf {
    let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    target.with_file_name(format!("{name}.pred-importem-{stamp}"))
}

/// Replace this context's settings with the bundle's.
///
/// **Never destructive without a copy.** Every file it is about to
/// overwrite is RENAMED aside first, and if any write then fails the whole
/// operation is rolled back: what was written is removed and every backup
/// is renamed back. The caller therefore sees either the new settings or
/// the old ones, never a config from the bundle beside a vault from the
/// machine — which is the one broken state that would leave connections
/// whose passwords cannot be found.
///
/// `history` and `params` are not in `plan` and are never touched.
pub fn apply(bundle: &Bundle, paths: &Paths) -> Result<Applied, StateError> {
    let mut plan: Vec<(PathBuf, &str)> = vec![(paths.config.clone(), bundle.config_toml.as_str())];
    if let Some(v) = &bundle.views_toml {
        plan.push((paths.views.clone(), v.as_str()));
    }
    if let Some(v) = &bundle.vault_bin {
        plan.push((paths.vault.clone(), v.as_str()));
    }

    let stamp = now_unix();
    let mut backed_up: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut written: Vec<PathBuf> = Vec::new();

    // Roll back to the state we found. Best effort by construction: we are
    // already on a failure path, and reporting the ORIGINAL failure matters
    // more than reporting a failure to undo it — the backups stay on disk
    // under their timestamped names either way, so nothing is lost even if
    // this cannot put them back.
    fn rollback(written: &[PathBuf], backed_up: &[(PathBuf, PathBuf)]) {
        for p in written {
            let _ = std::fs::remove_file(p);
        }
        for (target, bak) in backed_up {
            let _ = std::fs::rename(bak, target);
        }
    }

    for (target, _) in &plan {
        if target.exists() {
            let bak = backup_path(target, stamp);
            if let Err(e) = std::fs::rename(target, &bak) {
                rollback(&written, &backed_up);
                return Err(err(format!(
                    "{} nejde odložit stranou, nic se nezměnilo: {e}",
                    target.display()
                )));
            }
            backed_up.push((target.clone(), bak));
        }
    }

    for (target, content) in &plan {
        let attempt = (|| -> std::io::Result<()> {
            if let Some(dir) = target.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            let tmp = crate::fsutil::tmp_path_for(target);
            std::fs::write(&tmp, content)?;
            std::fs::rename(&tmp, target)
        })();
        if let Err(e) = attempt {
            rollback(&written, &backed_up);
            return Err(err(format!(
                "{} nejde zapsat — původní nastavení vráceno zpět: {e}",
                target.display()
            )));
        }
        written.push(target.clone());
    }

    Ok(Applied { written, backed_up })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Vault;

    /// Written as TOML rather than built from `AppConfig` so the fixture
    /// exercises the same parse the app does — and so `scripts_dir`, whose
    /// whole job here is to be STRIPPED, is genuinely present on disk.
    const ONE_CONNECTION_TOML: &str = r#"
scripts_dir = "C:/lokalni/skripty"

[[connections]]
id = "conn-abc"
name = "prodej"
engine = "sqlite"
host = ""
port = 0
database = "prodej.db"
user = ""
read_only = false
timeout_secs = 30
auto_limit = 1000
"#;

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            config: dir.join("config.toml"),
            vault: dir.join("vault.bin"),
            views: dir.join("views.toml"),
            params: dir.join("params.toml"),
            history: dir.join("history.sqlite"),
        }
    }

    /// A profile with one connection, one stored secret, one views file,
    /// plus the two things that must NOT travel.
    fn seed(dir: &Path, master: &str, secret: &str) -> Paths {
        let p = paths_in(dir);
        std::fs::write(&p.config, ONE_CONNECTION_TOML).unwrap();
        let mut v = Vault::create(&p.vault, master).unwrap();
        v.set_secret("conn-abc", secret).unwrap();
        std::fs::write(&p.views, "[\"conn-abc\\u001Fpublic\\u001Ft\"]\nhidden_columns = []\n")
            .unwrap();
        std::fs::write(&p.params, "# PARAM-MARKER\n").unwrap();
        std::fs::write(&p.history, "HISTORY-MARKER").unwrap();
        p
    }

    #[test]
    fn round_trip_carries_the_connections_and_a_vault_that_still_opens() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "master-heslo", "tajne-1");

        let bundle = build(&sp, "9.9.9").unwrap();
        let file = dst.path().join("prenos.dbcx");
        write(&bundle, &file).unwrap();
        let back = read(&file).unwrap();
        assert_eq!(back, bundle, "the file must survive a round trip verbatim");

        let dp = paths_in(dst.path());
        apply(&back, &dp).unwrap();

        let cfg = AppConfig::load(&dp.config).unwrap();
        assert_eq!(cfg.connections.len(), 1);
        assert_eq!(cfg.connections[0].name, "prodej");
        // The point of the whole exercise: the SOURCE master password opens
        // the imported vault, because nothing was re-encrypted.
        let v = Vault::unlock(&dp.vault, "master-heslo").unwrap();
        assert_eq!(v.get_secret("conn-abc").as_deref(), Some("tajne-1"));
        assert!(dp.views.exists());
    }

    /// The security rail. Written so it CANNOT pass vacuously: the two
    /// extra assertions prove the vault really was exported (its ciphertext
    /// is in the file) and that the secret really did travel (it comes back
    /// out after an import), so „the plaintext is absent" cannot be
    /// satisfied by exporting nothing at all.
    #[test]
    fn bundle_never_contains_a_plaintext_secret() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let needle = "SUPERTAJNE-NIKDY-V-EXPORTU";
        let sp = seed(src.path(), "master-heslo", needle);

        let bundle = build(&sp, "9.9.9").unwrap();
        let json = to_json(&bundle).unwrap();

        assert!(
            !json.contains(needle),
            "the exported bundle contains a stored password IN CLEAR — this is the one \
             outcome the format exists to prevent"
        );
        assert!(
            !json.contains("master-heslo"),
            "the exported bundle contains the master password"
        );

        // Non-vacuity 1: the vault is genuinely in there.
        let on_disk = std::fs::read_to_string(&sp.vault).unwrap();
        let env: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        let ct = env["ciphertext"].as_str().unwrap();
        assert!(json.contains(ct), "the vault ciphertext did not make it into the bundle");

        // Non-vacuity 2: the secret genuinely survives the trip, so its
        // absence above is about ENCRYPTION, not about omission.
        let dp = paths_in(dst.path());
        apply(&bundle, &dp).unwrap();
        let v = Vault::unlock(&dp.vault, "master-heslo").unwrap();
        assert_eq!(v.get_secret("conn-abc").as_deref(), Some(needle));
    }

    #[test]
    fn machine_local_paths_are_stripped() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "m", "s");
        let mut cfg = AppConfig::load(&sp.config).unwrap();
        cfg.tool_paths.psql = Some("C:/Program Files/psql.exe".to_string());
        std::fs::write(&sp.config, toml::to_string_pretty(&cfg).unwrap()).unwrap();
        // Non-vacuity: both really are in the file we are about to export.
        let raw = std::fs::read_to_string(&sp.config).unwrap();
        assert!(raw.contains("psql.exe") && raw.contains("lokalni"));

        let bundle = build(&sp, "9.9.9").unwrap();
        assert!(!bundle.config_toml.contains("psql.exe"), "tool_paths travelled");
        assert!(!bundle.config_toml.contains("lokalni"), "scripts_dir travelled");
        let exported: AppConfig = toml::from_str(&bundle.config_toml).unwrap();
        assert_eq!(exported.tool_paths, Default::default());
        assert_eq!(exported.scripts_dir, None);
        // …and the connection still made it, so the strip is targeted.
        assert_eq!(exported.connections.len(), 1);
    }

    #[test]
    fn history_and_params_stay_behind_and_are_never_overwritten() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "m", "s");
        let bundle = build(&sp, "9.9.9").unwrap();
        let json = to_json(&bundle).unwrap();
        assert!(!json.contains("PARAM-MARKER"), "params.toml travelled");
        assert!(!json.contains("HISTORY-MARKER"), "history travelled");

        // And on the way in they are left alone rather than deleted.
        let dp = seed(dst.path(), "jine-heslo", "jine-tajemstvi");
        std::fs::write(&dp.params, "# CIL-PARAM").unwrap();
        std::fs::write(&dp.history, "CIL-HISTORY").unwrap();
        apply(&bundle, &dp).unwrap();
        assert_eq!(std::fs::read_to_string(&dp.params).unwrap(), "# CIL-PARAM");
        assert_eq!(std::fs::read_to_string(&dp.history).unwrap(), "CIL-HISTORY");
    }

    #[test]
    fn apply_backs_up_everything_it_replaces() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "master-a", "tajne-a");
        let dp = seed(dst.path(), "master-b", "tajne-b");
        let bundle = build(&sp, "9.9.9").unwrap();

        let applied = apply(&bundle, &dp).unwrap();
        assert_eq!(applied.written.len(), 3);
        assert_eq!(applied.backed_up.len(), 3, "every replaced file needs a copy");
        for (target, bak) in &applied.backed_up {
            assert!(bak.exists(), "{} was replaced without a backup", target.display());
        }
        // The backed-up vault is still the TARGET's old one, openable with
        // the target's own master password — the whole point of the copy.
        let old_vault = &applied
            .backed_up
            .iter()
            .find(|(t, _)| t == &dp.vault)
            .expect("the vault was replaced, so it must have been backed up")
            .1;
        let v = Vault::unlock(old_vault, "master-b").unwrap();
        assert_eq!(v.get_secret("conn-abc").as_deref(), Some("tajne-b"));
    }

    /// A fresh profile has nothing to back up, and that must not be
    /// mistaken for a failure.
    #[test]
    fn importing_into_an_empty_profile_backs_up_nothing_and_still_writes() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "m", "s");
        let bundle = build(&sp, "9.9.9").unwrap();
        let dp = paths_in(dst.path());
        let applied = apply(&bundle, &dp).unwrap();
        assert!(applied.backed_up.is_empty());
        assert_eq!(applied.written.len(), 3);
    }

    #[test]
    fn a_failed_write_puts_the_previous_settings_back() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "master-a", "tajne-a");
        let bundle = build(&sp, "9.9.9").unwrap();

        let dp0 = seed(dst.path(), "master-b", "tajne-b");
        // `views` is written SECOND, after `config` has already landed.
        // Pointing it below a regular file makes `create_dir_all` fail, so
        // the rollback path runs with one file already written and three
        // already backed up.
        std::fs::write(dst.path().join("prekazka"), "not a directory").unwrap();
        let dp = Paths { views: dst.path().join("prekazka").join("views.toml"), ..dp0.clone() };

        let e = apply(&bundle, &dp).unwrap_err();
        assert!(e.message.contains("vráceno zpět"), "{}", e.message);

        // The target's own settings are back, untouched.
        let cfg = AppConfig::load(&dp.config).unwrap();
        assert_eq!(cfg.connections.len(), 1);
        let v = Vault::unlock(&dp.vault, "master-b").unwrap();
        assert_eq!(
            v.get_secret("conn-abc").as_deref(),
            Some("tajne-b"),
            "a half-applied import left the target's vault replaced"
        );
    }

    #[test]
    fn a_file_that_is_not_a_bundle_is_refused_by_name() {
        assert!(parse("{}").is_err());
        assert!(parse("nonsense").unwrap_err().message.contains("není soubor s nastavením"));
        let e = parse(r#"{"format":"neco-jineho","version":1,"config_toml":""}"#).unwrap_err();
        assert!(e.message.contains("cizí formát"), "{}", e.message);
    }

    #[test]
    fn a_bundle_from_a_newer_app_is_refused_instead_of_half_understood() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "m", "s");
        let mut bundle = build(&sp, "9.9.9").unwrap();
        bundle.version = VERSION + 1;
        let e = parse(&to_json(&bundle).unwrap()).unwrap_err();
        assert!(e.message.contains("novější"), "{}", e.message);
    }

    /// Validation happens on TEXT, before `apply` can overwrite anything —
    /// so a bundle carrying an unsealed vault never reaches the disk.
    #[test]
    fn a_bundle_whose_vault_is_not_sealed_is_refused_before_any_write() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "m", "s");
        let mut bundle = build(&sp, "9.9.9").unwrap();
        bundle.vault_bin = Some("heslo123".to_string());
        let e = parse(&to_json(&bundle).unwrap()).unwrap_err();
        assert!(e.message.contains("zašifrované obálky"), "{}", e.message);

        // The same shape on the way OUT: a vault path holding something
        // readable stops the export rather than exporting it.
        std::fs::write(&sp.vault, "moje-heslo-v-cistem-textu").unwrap();
        let e = build(&sp, "9.9.9").unwrap_err();
        assert!(e.message.contains("čitelného"), "{}", e.message);
    }

    #[test]
    fn a_bundle_with_unreadable_settings_inside_is_refused() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "m", "s");
        let mut bundle = build(&sp, "9.9.9").unwrap();
        bundle.config_toml = "tohle = neni [ toml".to_string();
        let e = parse(&to_json(&bundle).unwrap()).unwrap_err();
        assert!(e.message.contains("nejde přečíst"), "{}", e.message);
    }

    #[test]
    fn exporting_a_profile_with_no_connections_refuses_rather_than_making_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        std::fs::write(&p.config, toml::to_string_pretty(&AppConfig::default()).unwrap()).unwrap();
        let e = build(&p, "9.9.9").unwrap_err();
        assert!(e.message.contains("žádná uložená připojení"), "{}", e.message);
    }

    #[test]
    fn a_vault_that_does_not_exist_yet_is_simply_absent_from_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        std::fs::write(&p.config, ONE_CONNECTION_TOML).unwrap();
        let bundle = build(&p, "9.9.9").unwrap();
        assert_eq!(bundle.vault_bin, None);
        assert_eq!(bundle.views_toml, None);
        let s = summary(&bundle).unwrap();
        assert!(!s.has_vault);
        assert_eq!(s.connections, vec!["prodej".to_string()]);
    }

    #[test]
    fn the_summary_answers_the_dialogs_questions_without_a_master_password() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path(), "master-heslo", "tajne");
        let bundle = build(&sp, "0.27.0").unwrap();
        let s = summary(&bundle).unwrap();
        assert_eq!(s.connections, vec!["prodej".to_string()]);
        assert!(s.has_vault);
        assert!(s.has_views);
        assert_eq!(s.app_version, "0.27.0");
        assert!(s.created_unix > 1_700_000_000, "stamp looks wrong: {}", s.created_unix);
    }

    #[test]
    fn two_backups_of_the_same_file_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let a = backup_path(&dir.path().join("config.toml"), 111);
        let b = backup_path(&dir.path().join("config.toml"), 222);
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("config.toml."));
    }
}
