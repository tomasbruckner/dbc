//! Workspace folder — the ONE git-versioned folder that holds the whole
//! working context (design: `workspace-folder-design.md` Part W).
//!
//! Layout: `dbc-workspace.toml` (marker, `format = 1`), `config.toml`,
//! `vault.bin`, `views.toml`, `params.toml`, `scripts/`, `.gitignore`.
//! `history.sqlite` deliberately stays machine-local (§W5).
//!
//! Mode is decided by a POINTER file in the profile dir — never by
//! merging: exactly one context is active, and a pointer whose target is
//! unusable resolves to [`Resolution::Broken`], never silently to the
//! profile (§W4). Git stays entirely external: nothing here reads,
//! writes, or inspects anything under `.git/` (§W6.4).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::StateError;
use crate::fsutil::{entry_exists_ci, join_component, write_atomic};

/// Marker file in the workspace root — its presence is what makes a
/// folder a workspace, and it is written LAST by [`init_workspace`].
pub const MARKER_FILE: &str = "dbc-workspace.toml";
/// Pointer file in the PROFILE dir (it cannot live in the folder it
/// points at).
pub const POINTER_FILE: &str = "workspace.toml";
/// Fixed scripts subfolder (§W1): in workspace mode the scripts tree
/// always roots here, and `AppConfig::scripts_dir` is inert.
pub const SCRIPTS_SUBDIR: &str = "scripts";
/// Generated once at init, then owned by the user (§W6.2).
pub const GITIGNORE_FILE: &str = ".gitignore";
/// Layout format this build understands.
pub const WORKSPACE_FORMAT: u32 = 1;

/// Shipped `.gitignore` (§W6.2). The `vault.bin` line is COMMENTED OUT on
/// purpose: the user chose to version the vault, so the opt-out must be
/// discoverable without being imposed.
pub const GITIGNORE_TEMPLATE: &str = "# dbc workspace — pracovní prostor aplikace dbc.\n\
# Git zde spravujete výhradně vy; aplikace s gitem nikdy nepracuje.\n\
\n\
# Dočasné soubory atomických zápisů (po pádu aplikace mohou zůstat):\n\
*.toml.tmp\n\
*.bin.tmp\n\
\n\
# DOPORUČENÍ: vault.bin je šifrovaný trezor hesel (Argon2id).\n\
# Pokud ho NECHCETE verzovat (bezpečnější volba), odkomentujte\n\
# následující řádek. POZOR: historie gitu je trvalá — jednou\n\
# commitnutý trezor z ní nelze spolehlivě odstranit.\n\
# vault.bin\n";

/// The five persistent-store paths of ONE context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `AppConfig` — connections, favourites, theme, tool paths.
    pub config: PathBuf,
    /// Encrypted Argon2id vault — the ONLY file that may hold secrets.
    pub vault: PathBuf,
    /// Per-table view prefs (`ViewPrefsStore`).
    pub views: PathBuf,
    /// Last-used `:param` values (`ParamValuesStore`).
    pub params: PathBuf,
    /// Query history — machine-local in BOTH modes (§W5).
    pub history: PathBuf,
}

/// What a picked folder IS — decided before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Valid marker with a supported format — adopt it.
    Workspace,
    /// No entries, or only dot-entries (`.git`, `.gitignore`) — a fresh
    /// clone counts. Safe to initialize.
    Empty,
    /// Has real content but no marker — refuse (never scatter app files
    /// into someone's Documents folder).
    NonEmpty,
    /// Marker present, `format` newer than [`WORKSPACE_FORMAT`].
    FutureFormat(u32),
    /// The folder or its marker could not be read/parsed.
    Unreadable(String),
}

/// The active context, as resolved at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// No pointer — today's behavior, byte for byte.
    Profile(Paths),
    /// A pointer resolved to a real workspace folder.
    Workspace {
        /// The workspace folder itself.
        root: PathBuf,
        /// Its five store paths (history excepted, §W5).
        paths: Paths,
    },
    /// A pointer exists but its target is unusable. The caller MUST
    /// surface this and let the user choose (§W4) — falling back to the
    /// profile silently would put a muscle-memory click one step from the
    /// wrong „prod".
    Broken {
        /// The folder the pointer named, when it could be read at all.
        root: Option<PathBuf>,
        /// Display-ready Czech reason for the blocking modal.
        reason: String,
    },
}

#[derive(Serialize, Deserialize)]
struct Marker {
    format: u32,
}

#[derive(Serialize, Deserialize)]
struct PointerFile {
    path: String,
}

fn err(m: impl Into<String>) -> StateError {
    StateError { message: m.into() }
}

/// `%APPDATA%\dbc` (or the platform equivalent) — the profile dir every
/// `default_*_path()` already uses.
pub fn profile_dir() -> PathBuf {
    crate::config::default_config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `<profile>/workspace.toml`.
pub fn pointer_path() -> PathBuf {
    profile_dir().join(POINTER_FILE)
}

/// Today's paths, unchanged — the profile context.
pub fn profile_paths() -> Paths {
    Paths {
        config: crate::config::default_config_path(),
        vault: crate::vault::default_vault_path(),
        views: crate::view_prefs::default_view_prefs_path(),
        params: crate::params::default_param_values_path(),
        history: crate::history::default_history_path(),
    }
}

/// Workspace paths — everything in the folder EXCEPT history (§W5).
pub fn workspace_paths(root: &Path) -> Paths {
    Paths {
        config: root.join("config.toml"),
        vault: root.join("vault.bin"),
        views: root.join("views.toml"),
        params: root.join("params.toml"),
        history: crate::history::default_history_path(),
    }
}

/// Reads the pointer. Absent file ⇒ `Ok(None)`; unparsable ⇒ `Err`.
pub fn read_pointer(pointer: &Path) -> Result<Option<PathBuf>, StateError> {
    if !pointer.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(pointer)?;
    let p: PointerFile = toml::from_str(&raw)
        .map_err(|e| err(format!("ukazatel na pracovní prostor je poškozený: {e}")))?;
    Ok(Some(PathBuf::from(p.path)))
}

/// Writes the pointer atomically (shared rail).
pub fn write_pointer(pointer: &Path, root: &Path) -> Result<(), StateError> {
    if let Some(dir) = pointer.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(&PointerFile { path: root.display().to_string() })
        .map_err(|e| err(e.to_string()))?;
    write_atomic(pointer, body.as_bytes())
}

/// Removes the pointer (back to profile mode). Idempotent.
pub fn clear_pointer(pointer: &Path) -> Result<(), StateError> {
    match std::fs::remove_file(pointer) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Classifies a picked folder WITHOUT writing anything.
pub fn classify(root: &Path) -> Classification {
    if !root.is_dir() {
        return Classification::Unreadable("složka neexistuje".to_string());
    }
    let marker = root.join(MARKER_FILE);
    if marker.exists() {
        let raw = match std::fs::read_to_string(&marker) {
            Ok(r) => r,
            Err(e) => return Classification::Unreadable(format!("{MARKER_FILE}: {e}")),
        };
        return match toml::from_str::<Marker>(&raw) {
            Ok(m) if m.format <= WORKSPACE_FORMAT => Classification::Workspace,
            Ok(m) => Classification::FutureFormat(m.format),
            Err(e) => Classification::Unreadable(format!("{MARKER_FILE}: {e}")),
        };
    }
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) => return Classification::Unreadable(e.to_string()),
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        // Dot-entries only (a fresh clone: `.git`, `.gitignore`) still
        // counts as empty. NOTHING under `.git/` is ever opened (§W6.4).
        if !name.starts_with('.') {
            return Classification::NonEmpty;
        }
    }
    Classification::Empty
}

/// Resolves the active context from a pointer file path (testable core).
pub fn resolve_at(pointer: &Path) -> Resolution {
    let root = match read_pointer(pointer) {
        Ok(None) => return Resolution::Profile(profile_paths()),
        Ok(Some(r)) => r,
        Err(e) => return Resolution::Broken { root: None, reason: e.message },
    };
    match classify(&root) {
        Classification::Workspace => {
            let paths = workspace_paths(&root);
            Resolution::Workspace { root, paths }
        }
        Classification::Empty | Classification::NonEmpty => {
            Resolution::Broken { root: Some(root), reason: format!("chybí {MARKER_FILE}") }
        }
        Classification::FutureFormat(f) => Resolution::Broken {
            root: Some(root),
            reason: format!("pracovní prostor vyžaduje novější verzi aplikace (formát {f})"),
        },
        Classification::Unreadable(m) => Resolution::Broken { root: Some(root), reason: m },
    }
}

/// Resolves from the real pointer location.
pub fn resolve() -> Resolution {
    resolve_at(&pointer_path())
}

/// Copies one file if the source exists; refuses to overwrite an existing
/// destination (case-insensitively — the shared rail), so init can never
/// destroy something the user already had.
fn copy_one(src: &Path, root: &Path, name: &str) -> Result<(), StateError> {
    if !src.exists() {
        return Ok(());
    }
    if entry_exists_ci(root, name)? {
        return Err(err(format!("cíl už existuje: {name}")));
    }
    let dst = join_component(root, name)?;
    let bytes = std::fs::read(src)?;
    write_atomic(&dst, &bytes)
}

/// Steps 1–4 of [`init_workspace`] — the copies, `scripts/` and the
/// `.gitignore` — WITHOUT the marker. Separate so the marker-last
/// ordering is a testable property, not a comment (§W3.2).
pub fn init_contents(root: &Path, from: &Paths) -> Result<(), StateError> {
    if !root.is_dir() {
        return Err(err("složka neexistuje"));
    }
    copy_one(&from.config, root, "config.toml")?;
    copy_one(&from.vault, root, "vault.bin")?;
    copy_one(&from.views, root, "views.toml")?;
    copy_one(&from.params, root, "params.toml")?;
    // history.sqlite is deliberately NOT copied (§W5).
    let scripts = join_component(root, SCRIPTS_SUBDIR)?;
    if !scripts.is_dir() {
        std::fs::create_dir(&scripts).map_err(|e| err(format!("vytvoření složky selhalo: {e}")))?;
    }
    if !entry_exists_ci(root, GITIGNORE_FILE)? {
        let gi = join_component(root, GITIGNORE_FILE)?;
        write_atomic(&gi, GITIGNORE_TEMPLATE.as_bytes())?;
    }
    Ok(())
}

/// Writes the marker — the COMMIT POINT of an init. A crash before this
/// leaves a folder that [`classify`] calls `NonEmpty`, i.e. not
/// adoptable, with the profile still fully intact.
pub fn write_marker(root: &Path) -> Result<(), StateError> {
    let body =
        toml::to_string(&Marker { format: WORKSPACE_FORMAT }).map_err(|e| err(e.to_string()))?;
    let path = join_component(root, MARKER_FILE)?;
    write_atomic(&path, body.as_bytes())
}

/// Initializes an EMPTY folder into a workspace: copy, `scripts/`,
/// `.gitignore`, then the marker LAST. Never deletes, moves, or modifies
/// anything in the profile dir (§W3.2). Does NOT write the pointer — the
/// caller does that only after this returns `Ok`.
pub fn init_workspace(root: &Path, from: &Paths) -> Result<(), StateError> {
    init_contents(root, from)?;
    write_marker(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake "profile" directory with all four copyable files present.
    fn fake_profile(dir: &Path) -> Paths {
        std::fs::write(dir.join("config.toml"), "theme = \"dark\"\n").unwrap();
        std::fs::write(dir.join("vault.bin"), b"\x00binary-envelope-SUPERTAJNE123").unwrap();
        std::fs::write(dir.join("views.toml"), "").unwrap();
        std::fs::write(dir.join("params.toml"), "").unwrap();
        std::fs::write(dir.join("history.sqlite"), b"sqlite").unwrap();
        Paths {
            config: dir.join("config.toml"),
            vault: dir.join("vault.bin"),
            views: dir.join("views.toml"),
            params: dir.join("params.toml"),
            history: dir.join("history.sqlite"),
        }
    }

    #[test]
    fn profile_paths_are_exactly_todays_defaults() {
        let p = profile_paths();
        assert_eq!(p.config, crate::config::default_config_path());
        assert_eq!(p.vault, crate::vault::default_vault_path());
        assert_eq!(p.views, crate::view_prefs::default_view_prefs_path());
        assert_eq!(p.params, crate::params::default_param_values_path());
        assert_eq!(p.history, crate::history::default_history_path());
    }

    #[test]
    fn workspace_paths_live_in_the_folder_except_history() {
        let root = Path::new("D:\\ws");
        let p = workspace_paths(root);
        assert_eq!(p.config, root.join("config.toml"));
        assert_eq!(p.vault, root.join("vault.bin"));
        assert_eq!(p.views, root.join("views.toml"));
        assert_eq!(p.params, root.join("params.toml"));
        // Design §W5: a binary SQLite rewritten per query is a git conflict
        // factory — history stays machine-local.
        assert_eq!(p.history, crate::history::default_history_path());
    }

    #[test]
    fn pointer_roundtrips_and_absent_is_none() {
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        assert_eq!(read_pointer(&ptr).unwrap(), None);
        write_pointer(&ptr, Path::new("D:\\ws")).unwrap();
        assert_eq!(read_pointer(&ptr).unwrap(), Some(PathBuf::from("D:\\ws")));
        clear_pointer(&ptr).unwrap();
        assert_eq!(read_pointer(&ptr).unwrap(), None);
        clear_pointer(&ptr).unwrap(); // idempotent
    }

    #[test]
    fn resolve_without_pointer_is_profile() {
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        assert!(matches!(resolve_at(&ptr), Resolution::Profile(_)));
    }

    #[test]
    fn resolve_with_marker_is_workspace() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        write_marker(&root).unwrap();
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, &root).unwrap();
        match resolve_at(&ptr) {
            Resolution::Workspace { root: r, paths } => {
                assert_eq!(r, root);
                assert_eq!(paths.config, root.join("config.toml"));
            }
            other => panic!("expected Workspace, got {other:?}"),
        }
    }

    #[test]
    fn missing_folder_is_broken_never_profile() {
        // The never-silent-fallback rail (design §W4).
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, &td.path().join("gone")).unwrap();
        match resolve_at(&ptr) {
            Resolution::Broken { root, reason } => {
                assert_eq!(root, Some(td.path().join("gone")));
                assert_eq!(reason, "složka neexistuje");
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn folder_without_marker_is_broken() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("config.toml"), "").unwrap();
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, &root).unwrap();
        match resolve_at(&ptr) {
            Resolution::Broken { reason, .. } => assert_eq!(reason, "chybí dbc-workspace.toml"),
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn future_format_is_broken_with_its_own_reason() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(MARKER_FILE), "format = 99\n").unwrap();
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, &root).unwrap();
        match resolve_at(&ptr) {
            Resolution::Broken { reason, .. } => {
                assert_eq!(reason, "pracovní prostor vyžaduje novější verzi aplikace (formát 99)");
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_pointer_is_broken_without_a_root() {
        // Never a silent fallback, even when we cannot say WHERE it pointed.
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        std::fs::write(&ptr, "path = [not toml").unwrap();
        match resolve_at(&ptr) {
            Resolution::Broken { root, reason } => {
                assert_eq!(root, None);
                assert!(
                    reason.starts_with("ukazatel na pracovní prostor je poškozený:"),
                    "{reason}"
                );
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn classify_empty_and_dot_only_folders_are_empty() {
        let td = tempfile::tempdir().unwrap();
        let a = td.path().join("a");
        std::fs::create_dir(&a).unwrap();
        assert_eq!(classify(&a), Classification::Empty);

        // A fresh clone of an empty private repo: only dot-entries.
        let b = td.path().join("b");
        std::fs::create_dir(&b).unwrap();
        std::fs::create_dir(b.join(".git")).unwrap();
        std::fs::write(b.join(".gitignore"), "").unwrap();
        assert_eq!(classify(&b), Classification::Empty);
    }

    #[test]
    fn classify_non_empty_without_marker_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let d = td.path().join("documents");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("dopis.docx"), "").unwrap();
        assert_eq!(classify(&d), Classification::NonEmpty);
    }

    #[test]
    fn init_copies_profile_files_and_leaves_the_originals_untouched() {
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();

        init_workspace(&root, &from).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("config.toml")).unwrap(),
            "theme = \"dark\"\n"
        );
        assert_eq!(
            std::fs::read(root.join("vault.bin")).unwrap(),
            std::fs::read(&from.vault).unwrap()
        );
        assert!(root.join("views.toml").exists());
        assert!(root.join("params.toml").exists());
        assert!(root.join(SCRIPTS_SUBDIR).is_dir());
        assert!(root.join(GITIGNORE_FILE).exists());
        assert_eq!(classify(&root), Classification::Workspace);

        // NEVER DESTRUCTIVE: every profile file still there, byte-identical.
        assert!(
            from.config.exists() && from.vault.exists() && from.views.exists()
                && from.params.exists()
        );
        assert_eq!(std::fs::read_to_string(&from.config).unwrap(), "theme = \"dark\"\n");
        // History is NOT copied (design §W5).
        assert!(!root.join("history.sqlite").exists());
    }

    #[test]
    fn init_skips_profile_files_that_do_not_exist() {
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        // A user who never saved a password has no vault.bin.
        std::fs::write(prof.join("config.toml"), "").unwrap();
        let from = Paths {
            config: prof.join("config.toml"),
            vault: prof.join("vault.bin"),
            views: prof.join("views.toml"),
            params: prof.join("params.toml"),
            history: prof.join("history.sqlite"),
        };
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        init_workspace(&root, &from).unwrap();
        assert!(root.join("config.toml").exists());
        assert!(!root.join("vault.bin").exists());
        assert_eq!(classify(&root), Classification::Workspace);
    }

    #[test]
    fn a_crash_before_the_marker_leaves_a_non_adoptable_folder() {
        // Crash-safety pin (design §W3.2): the marker is the commit point.
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();

        init_contents(&root, &from).unwrap(); // everything EXCEPT write_marker

        assert!(root.join("config.toml").exists(), "contents did copy");
        assert!(!root.join(MARKER_FILE).exists(), "marker must not exist yet");
        assert_eq!(classify(&root), Classification::NonEmpty);
        // And the profile is untouched, so the user loses nothing.
        assert!(from.config.exists() && from.vault.exists());
    }

    #[test]
    fn a_failing_init_writes_no_marker() {
        // Deterministic failure injection: `scripts` already exists as a FILE,
        // so `create_dir` fails mid-init.
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(SCRIPTS_SUBDIR), b"not a directory").unwrap();

        assert!(init_workspace(&root, &from).is_err());
        assert!(!root.join(MARKER_FILE).exists());
        assert_ne!(classify(&root), Classification::Workspace);
        assert!(from.config.exists() && from.vault.exists());
    }

    #[test]
    fn init_refuses_to_overwrite_an_existing_file_case_insensitively() {
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("Config.TOML"), "PRECIOUS").unwrap();

        assert!(init_workspace(&root, &from).is_err());
        assert_eq!(std::fs::read_to_string(root.join("Config.TOML")).unwrap(), "PRECIOUS");
        assert!(!root.join(MARKER_FILE).exists());
    }

    #[test]
    fn init_never_overwrites_an_existing_gitignore() {
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(GITIGNORE_FILE), "# mine\n").unwrap();

        init_workspace(&root, &from).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(GITIGNORE_FILE)).unwrap(), "# mine\n");
    }

    #[test]
    fn init_refuses_a_root_that_is_not_a_directory() {
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let gone = td.path().join("neni");
        assert_eq!(init_workspace(&gone, &from).unwrap_err().message, "složka neexistuje");
    }

    #[test]
    fn gitignore_template_is_byte_pinned() {
        // Deliverable text (design §W6.2): the commented-out vault line is
        // the user's one-character opt-out — it must stay commented, stay
        // spelled `vault.bin`, and keep its permanence warning.
        assert_eq!(
            GITIGNORE_TEMPLATE,
            "# dbc workspace — pracovní prostor aplikace dbc.\n\
             # Git zde spravujete výhradně vy; aplikace s gitem nikdy nepracuje.\n\
             \n\
             # Dočasné soubory atomických zápisů (po pádu aplikace mohou zůstat):\n\
             *.toml.tmp\n\
             *.bin.tmp\n\
             \n\
             # DOPORUČENÍ: vault.bin je šifrovaný trezor hesel (Argon2id).\n\
             # Pokud ho NECHCETE verzovat (bezpečnější volba), odkomentujte\n\
             # následující řádek. POZOR: historie gitu je trvalá — jednou\n\
             # commitnutý trezor z ní nelze spolehlivě odstranit.\n\
             # vault.bin\n"
        );
    }

    #[test]
    fn marker_holds_only_the_format_key() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        write_marker(&root).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(MARKER_FILE)).unwrap(), "format = 1\n");
    }

    #[test]
    fn no_plaintext_secret_is_written_outside_vault_bin() {
        // Security rail (design §W6.5): the ONLY file in the workspace that
        // may contain secret material is vault.bin (encrypted at that).
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof); // vault.bin contains "SUPERTAJNE123"
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        init_workspace(&root, &from).unwrap();

        for ent in std::fs::read_dir(&root).unwrap() {
            let ent = ent.unwrap();
            if ent.file_type().unwrap().is_dir() || ent.file_name() == "vault.bin" {
                continue;
            }
            let raw = std::fs::read(ent.path()).unwrap();
            assert!(
                !raw.windows(13).any(|w| w == b"SUPERTAJNE123"),
                "secret material leaked into {:?}",
                ent.file_name()
            );
        }
    }

    #[test]
    fn the_pointer_file_carries_no_secret_and_only_the_path() {
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, Path::new("D:\\tajny prostor")).unwrap();
        let raw = std::fs::read_to_string(&ptr).unwrap();
        assert!(raw.contains("path ="), "{raw}");
        for forbidden in ["password", "heslo", "vault", "master"] {
            assert!(!raw.contains(forbidden), "pointer must not carry {forbidden}: {raw}");
        }
    }
}
