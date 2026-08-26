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
///
/// The tmp rule is the BLANKET `*.tmp`, not an enumeration of extensions.
/// [`crate::fsutil::write_atomic`] names its temporary sibling
/// `<path>.tmp` for whatever it is given, so the set of suffixes that can
/// appear is exactly "every file the app writes": `*.toml.tmp`,
/// `*.bin.tmp`, `*.sql.tmp` (by far the most frequent — a crash during
/// Ctrl+S in the scripts library), and even `.gitignore.tmp`. An
/// enumeration silently stops covering the next store that is added;
/// `*.tmp` cannot drift out of date, and the file is the user's to edit
/// from the moment it is written.
pub const GITIGNORE_TEMPLATE: &str = "# dbc workspace — pracovní prostor aplikace dbc.\n\
# Git zde spravujete výhradně vy; aplikace s gitem nikdy nepracuje.\n\
\n\
# Dočasné soubory atomických zápisů (po pádu aplikace mohou zůstat).\n\
# Aplikace je vždy pojmenuje <soubor>.tmp, proto jediné pravidlo:\n\
*.tmp\n\
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

/// Collapse a [`Resolution::Broken`] `reason` to ONE line: its first and
/// last non-empty lines, joined — everything between them dropped.
///
/// T10 carry-forward 5. This lived in `dbc-mcp` only, so the two surfaces
/// that display a broken pointer disagreed: the server printed two tidy
/// stderr lines while the BLOCKING GUI modal (`connections_ui`'s
/// `render_workspace_missing_panel`, §W4 — the one dialog the user cannot
/// Esc out of) rendered the raw reason. It now lives beside the
/// `Resolution` that PRODUCES the reason, which is the only place that
/// knows what shapes a reason can take, and both consumers call it.
///
/// Why it is needed at all: the likeliest broken-pointer case is an
/// unparsable `workspace.toml`, and `toml::de::Error`'s `Display` is
/// multi-line — `<position>\n<source-echo art>\n<explanation>`, eight
/// lines ending in an orphaned `)`. The FIRST and LAST non-empty lines are
/// exactly the two useful parts (where, and what); everything between is
/// the `|`/`^` art, which also ECHOES THE POINTER'S SOURCE TEXT — so
/// dropping it is a small privacy win as well as a legibility one. A
/// single-line reason (every other `Broken` case, e.g. „složka
/// neexistuje") is returned unchanged.
///
/// Also applied to the DISPLAYED PATH by both callers: the pointer's
/// `path` field is arbitrary TOML text that is never validated as a real
/// path, so a hand-edited pointer containing a `\n` escape would otherwise
/// put attacker-chosen text on its own line — falsifying `dbc-mcp`'s
/// "exactly two lines on stderr" property (T10 carry-forward 3).
pub fn one_line_reason(reason: &str) -> String {
    let mut lines = reason.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else { return String::new() };
    match lines.last() {
        Some(last) => format!("{first}: {last}"),
        None => first.to_string(),
    }
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

/// Reads the pointer. Genuinely ABSENT file ⇒ `Ok(None)`; unreadable or
/// unparsable ⇒ `Err` (which [`resolve_at`] turns into
/// [`Resolution::Broken`]).
///
/// The existence check is [`Path::try_exists`], NOT `Path::exists`:
/// `exists()` collapses every `io::Error` into `false`, so a pointer that
/// IS there but cannot be probed (deny-read ACL from a corporate GPO or a
/// mis-applied `icacls`, a dangling reparse point) would read as "no
/// pointer" and silently drop the user into profile mode — the exact
/// fallback §W4 exists to prevent.
pub fn read_pointer(pointer: &Path) -> Result<Option<PathBuf>, StateError> {
    read_pointer_probed(pointer, pointer.try_exists())
}

/// Testable seam for [`read_pointer`]: the existence probe is injected,
/// so the "pointer is there but unreadable" branch can be pinned without
/// needing a genuinely deny-read file on every platform CI runs on.
fn read_pointer_probed(
    pointer: &Path,
    probe: std::io::Result<bool>,
) -> Result<Option<PathBuf>, StateError> {
    match probe {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(e) => {
            return Err(err(format!(
                "ukazatel na pracovní prostor nelze ověřit ({}): {e}",
                pointer.display()
            )));
        }
    }
    let raw = std::fs::read_to_string(pointer)?;
    let p: PointerFile = toml::from_str(&raw)
        .map_err(|e| err(format!("ukazatel na pracovní prostor je poškozený: {e}")))?;
    Ok(Some(PathBuf::from(p.path)))
}

/// Writes the pointer atomically (shared rail).
///
/// `root` must be ABSOLUTE and representable as UTF-8: the pointer is
/// read back in a different process, from a different working directory,
/// and the TOML body carries the path as a string. A relative or lossily
/// convertible path would round-trip into a DIFFERENT folder, which is
/// the silent-wrong-context direction — so it is refused at the rail
/// instead of being papered over by `display().to_string()`.
pub fn write_pointer(pointer: &Path, root: &Path) -> Result<(), StateError> {
    if !root.is_absolute() {
        return Err(err(format!(
            "cesta k pracovnímu prostoru musí být absolutní: {}",
            root.display()
        )));
    }
    let Some(root_str) = root.to_str() else {
        return Err(err(format!(
            "cesta k pracovnímu prostoru obsahuje znaky, které nelze uložit: {}",
            root.display()
        )));
    };
    if let Some(dir) = pointer.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(&PointerFile { path: root_str.to_string() })
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
        // "Not a directory" has three distinct causes and the user needs
        // to be told WHICH: a path that is gone, a path that is a FILE
        // (typing `…\ws\config.toml` into the picker), and a path that
        // cannot be probed at all.
        return match root.try_exists() {
            Ok(true) => Classification::Unreadable("cesta není složka".to_string()),
            Ok(false) => Classification::Unreadable("složka neexistuje".to_string()),
            Err(e) => Classification::Unreadable(format!("složku nelze ověřit: {e}")),
        };
    }
    let marker = root.join(MARKER_FILE);
    let probe = marker.try_exists();
    classify_probed(root, &marker, probe)
}

/// Testable seam for [`classify`]: the MARKER existence probe is injected,
/// so the "marker is there but cannot even be probed" branch can be pinned
/// without needing a genuinely deny-read file on every platform CI runs on
/// (same seam shape as [`read_pointer_probed`]).
///
/// The probe is [`Path::try_exists`], NOT `Path::exists`: `exists()`
/// collapses every `io::Error` into `false`, which here would report a
/// deny-read marker as „chybí dbc-workspace.toml". Still `Broken` either
/// way — but with a reason that sends the user hunting for a file that is
/// sitting right there. A wrong diagnosis is its own defect.
fn classify_probed(root: &Path, marker: &Path, probe: std::io::Result<bool>) -> Classification {
    match probe {
        Ok(true) => {}
        Ok(false) => return classify_unmarked(root),
        Err(e) => {
            return Classification::Unreadable(format!("{MARKER_FILE} nelze ověřit: {e}"));
        }
    }
    let raw = match std::fs::read_to_string(marker) {
        Ok(r) => r,
        Err(e) => return Classification::Unreadable(format!("{MARKER_FILE}: {e}")),
    };
    match toml::from_str::<Marker>(&raw) {
        // Formats are 1-based: no build ever wrote `format = 0`, so a
        // zero is a hand-edited or truncated marker, NOT an old layout to
        // be adopted silently.
        Ok(m) if (1..=WORKSPACE_FORMAT).contains(&m.format) => Classification::Workspace,
        Ok(m) if m.format > WORKSPACE_FORMAT => Classification::FutureFormat(m.format),
        Ok(m) => Classification::Unreadable(format!("{MARKER_FILE}: neplatný formát {}", m.format)),
        Err(e) => Classification::Unreadable(format!("{MARKER_FILE}: {e}")),
    }
}

/// A directory with no marker: `Empty` (nothing, or only dot-entries — a
/// fresh clone) or `NonEmpty` (real content we must never scatter into).
fn classify_unmarked(root: &Path) -> Classification {
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
    resolve_from_pointer(read_pointer(pointer))
}

/// The pointer-read → [`Resolution`] mapping, split out so the
/// never-silently-`Profile` rail can be pinned for reads that no portable
/// test can provoke on disk. `Ok(None)` — and ONLY `Ok(None)` — is
/// profile mode.
fn resolve_from_pointer(read: Result<Option<PathBuf>, StateError>) -> Resolution {
    let root = match read {
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
///
/// PRECONDITION, checked here and not merely by the caller:
/// [`classify`] must say [`Classification::Empty`]. `copy_one` refuses to
/// overwrite, but it refuses ONE file at a time — a destination holding
/// only `params.toml` would pass the first two copies and fail on the
/// third, leaving `config.toml` and a full copy of the encrypted
/// `vault.bin` scattered in a folder the user may not be tracking.
/// Nothing is destroyed, but a stray vault copy is not debris we get to
/// leave behind. The guard is the rail BEHIND the caller's own check, so
/// a future second caller cannot reintroduce the hole.
pub fn init_contents(root: &Path, from: &Paths) -> Result<(), StateError> {
    match classify(root) {
        Classification::Empty => {}
        // Preserves the pre-existing "složka neexistuje" wording for a
        // missing root (and now says something truer for a file/probe
        // failure).
        Classification::Unreadable(m) => return Err(err(m)),
        Classification::Workspace | Classification::FutureFormat(_) => {
            return Err(err("složka už je pracovní prostor"));
        }
        Classification::NonEmpty => return Err(err("složka není prázdná")),
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

    /// T10 carry-forward 5 — the tests that moved here with the function.
    /// Both display surfaces (`dbc-mcp`'s stderr, the §W4 blocking modal)
    /// now depend on this shape, so it is pinned in the crate that owns it
    /// rather than in one of the two consumers.
    #[test]
    fn one_line_reason_keeps_where_and_what_and_drops_the_toml_art() {
        // Verbatim shape of a real `toml` parse error.
        let reason = "TOML parse error at line 1, column 12\n  \
                      |\n1 | path = \"D:\\ws-gone\"\n  |            ^\n\
                      missing escaped value, expected `b`";
        let out = one_line_reason(reason);
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("TOML parse error at line 1, column 12"), "keeps WHERE: {out}");
        assert!(out.contains("missing escaped value, expected `b`"), "keeps WHAT: {out}");
        // The dropped art ECHOES THE POINTER SOURCE — worth its own line.
        assert!(!out.contains("path = "), "drops toml's source echo: {out}");
        assert!(!out.contains('^'), "drops the ascii art: {out}");
    }

    #[test]
    fn one_line_reason_passes_a_single_line_through_and_survives_the_empty_case() {
        assert_eq!(one_line_reason("složka neexistuje"), "složka neexistuje");
        // Trimmed, but not reworded.
        assert_eq!(one_line_reason("  složka neexistuje  "), "složka neexistuje");
        assert_eq!(one_line_reason(""), "");
        assert_eq!(one_line_reason("\n\n   \n"), "");
        // Exactly two useful lines: nothing is between them to drop.
        assert_eq!(one_line_reason("a\nb"), "a: b");
    }

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
    fn an_unreadable_pointer_is_broken_never_profile() {
        // THE rail (design §W4). `Path::exists()` collapses EVERY io::Error
        // into `false`, so a pointer that is on disk but cannot be probed
        // (deny-read ACL from a corporate GPO, a mis-applied `icacls`, a
        // dangling reparse point) used to read as "no pointer at all" and
        // hand the user profile-mode connections without a single word —
        // one muscle-memory click from the wrong „prod". No portable test
        // can create such a file on every platform, so the probe is
        // injected at the seam; what is pinned is that an `Err` NEVER
        // becomes `Profile`.
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        let denied = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access is denied. (os error 5)",
        );

        let read = read_pointer_probed(&ptr, Err(denied));
        let e = read.as_ref().unwrap_err();
        assert!(e.message.contains("nelze ověřit"), "{}", e.message);
        assert!(e.message.contains("workspace.toml"), "must name the pointer: {}", e.message);

        match resolve_from_pointer(read) {
            Resolution::Broken { root, reason } => {
                assert_eq!(root, None);
                assert!(reason.contains("nelze ověřit"), "{reason}");
            }
            other => panic!("an unreadable pointer must NEVER resolve to Profile, got {other:?}"),
        }

        // And the two probe outcomes that are NOT errors still behave.
        assert_eq!(read_pointer_probed(&ptr, Ok(false)).unwrap(), None);
        assert!(matches!(
            resolve_from_pointer(read_pointer_probed(&ptr, Ok(false))),
            Resolution::Profile(_)
        ));
    }

    #[test]
    fn a_pointer_that_is_a_directory_is_broken_not_profile() {
        // The same rail, provoked for real and portably: the pointer path
        // EXISTS (so `try_exists` says true) but `read_to_string` cannot
        // read it.
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        std::fs::create_dir(&ptr).unwrap();
        assert!(read_pointer(&ptr).is_err(), "a directory pointer must not read as None");
        assert!(
            matches!(resolve_at(&ptr), Resolution::Broken { .. }),
            "expected Broken, never Profile"
        );
    }

    #[test]
    fn read_pointer_probes_with_try_exists_not_exists() {
        // REGRESSION PIN for the probe choice itself. The two tests above
        // both survive a revert of `try_exists()` back to `exists()` — one
        // injects at the `read_pointer_probed` seam (bypassing the probe
        // entirely), the other uses a directory (which `exists()` reports
        // as `true` just the same). This one does not: an interior-NUL path
        // makes `exists()` return `false` (⇒ `Ok(None)` ⇒ silent PROFILE
        // mode) while `try_exists()` returns `Err(InvalidInput)` (⇒ Broken).
        let ptr = Path::new("a\u{0}b.toml");
        assert!(read_pointer(ptr).is_err(), "exists() would silently report no pointer");
        assert!(matches!(resolve_at(ptr), Resolution::Broken { .. }));
    }

    #[test]
    fn an_unprobeable_marker_says_so_instead_of_chybi() {
        // T4 review NIT-10. `classify` used `marker.exists()`, which
        // collapses every io::Error into `false` — a deny-read marker
        // (corporate ACL, dangling reparse point) was therefore reported as
        // „chybí dbc-workspace.toml", sending the user to look for a file
        // that is sitting right there. Still Broken either way; a wrong
        // diagnosis is its own defect. No portable test can create such a
        // file, so the probe is injected at the seam.
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let marker = root.join(MARKER_FILE);
        let denied =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Access is denied.");

        match classify_probed(&root, &marker, Err(denied)) {
            Classification::Unreadable(m) => {
                assert!(m.contains("nelze ověřit"), "{m}");
                assert!(!m.contains("chybí"), "must not claim the marker is missing: {m}");
                assert!(m.contains(MARKER_FILE), "must name the file: {m}");
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }

        // The two non-error probe outcomes are unchanged.
        assert_eq!(classify_probed(&root, &marker, Ok(false)), Classification::Empty);
        write_marker(&root).unwrap();
        assert_eq!(classify_probed(&root, &marker, Ok(true)), Classification::Workspace);
        assert_eq!(classify(&root), Classification::Workspace);
    }

    #[test]
    fn write_pointer_refuses_a_relative_root() {
        // The pointer is read back from another process with another CWD:
        // a relative path would round-trip into a DIFFERENT folder.
        let td = tempfile::tempdir().unwrap();
        let ptr = td.path().join("workspace.toml");
        let e = write_pointer(&ptr, Path::new("ws")).unwrap_err();
        assert!(e.message.contains("absolutní"), "{}", e.message);
        assert!(!ptr.exists(), "nothing may be written for a refused root");
        // Refused at the rail ⇒ still no pointer ⇒ still profile mode.
        assert!(matches!(resolve_at(&ptr), Resolution::Profile(_)));
    }

    #[test]
    fn a_pointer_aimed_at_a_file_says_so_instead_of_composing_neexistuje() {
        let td = tempfile::tempdir().unwrap();
        let target = td.path().join("config.toml");
        std::fs::write(&target, "").unwrap();
        assert_eq!(classify(&target), Classification::Unreadable("cesta není složka".to_string()));
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, &target).unwrap();
        match resolve_at(&ptr) {
            Resolution::Broken { root, reason } => {
                assert_eq!(root, Some(target));
                assert_eq!(reason, "cesta není složka");
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn format_zero_is_not_a_workspace() {
        // No build ever wrote `format = 0`; a zero is a hand-edited or
        // truncated marker, not an older layout to adopt silently.
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(MARKER_FILE), "format = 0\n").unwrap();
        assert_eq!(
            classify(&root),
            Classification::Unreadable(format!("{MARKER_FILE}: neplatný formát 0"))
        );
        let ptr = td.path().join("workspace.toml");
        write_pointer(&ptr, &root).unwrap();
        assert!(matches!(resolve_at(&ptr), Resolution::Broken { .. }));
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
        // Deterministic failure injection AFTER the Empty precondition: the
        // destination is genuinely empty, but the SOURCE `views.toml` is a
        // directory, so `copy_one`'s `fs::read` fails on the THIRD copy.
        // (The old injection — `scripts` pre-existing as a file — is now
        // caught one step earlier by the precondition, so it no longer
        // reaches the marker-ordering code this test is about.)
        //
        // Injecting at `views` and not at `config` is the point: two copies
        // LAND first, so the folder holds real partial state when the
        // failure hits. That is what makes this a marker-LAST pin rather
        // than a "nothing happened at all" pin.
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let mut from = fake_profile(&prof);
        std::fs::create_dir(prof.join("views-dir")).unwrap();
        from.views = prof.join("views-dir");
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();

        assert!(init_workspace(&root, &from).is_err());
        // Real partial state on disk — the copies before the failure DID
        // land, and the marker still did not.
        assert!(root.join("config.toml").exists(), "the first copy landed");
        assert!(root.join("vault.bin").exists(), "the second copy landed");
        assert!(!root.join("views.toml").exists(), "the failing copy did not");
        assert!(!root.join(MARKER_FILE).exists());
        assert_ne!(classify(&root), Classification::Workspace);
        assert_eq!(classify(&root), Classification::NonEmpty, "not adoptable");
        assert!(from.config.exists() && from.vault.exists());
    }

    #[test]
    fn init_refuses_a_non_empty_folder_before_touching_the_users_file() {
        // TWO rails now stand between an init and a user's file: the
        // `classify(root) == Empty` precondition (which fires first, before
        // ANY byte is written — that is what this test pins) and
        // `copy_one`'s case-insensitive `entry_exists_ci` refusal behind it
        // (pinned directly in `fsutil::tests`). What this test owns is the
        // OUTCOME: the user's `Config.TOML` survives byte-identical, no
        // marker appears, and nothing new is written beside it.
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
        // The precondition fired BEFORE any byte was written: the user's
        // file is still the only entry in the folder.
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["Config.TOML".to_string()]);
    }

    #[test]
    fn init_into_a_non_empty_folder_leaves_no_debris_not_even_a_vault_copy() {
        // The finding this pins: without the `Empty` precondition, a
        // destination whose only clash is `params.toml` sails through the
        // config.toml and vault.bin copies and only THEN fails — leaving a
        // full copy of the encrypted vault in a folder the user may not be
        // tracking. Nothing is destroyed either way; the point is that
        // nothing is SCATTERED either.
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("params.toml"), "MOJE").unwrap();

        let e = init_workspace(&root, &from).unwrap_err();
        assert_eq!(e.message, "složka není prázdná");

        assert!(!root.join("config.toml").exists(), "config.toml debris");
        assert!(!root.join("vault.bin").exists(), "VAULT COPY debris");
        assert!(!root.join("views.toml").exists());
        assert!(!root.join(SCRIPTS_SUBDIR).exists());
        assert!(!root.join(GITIGNORE_FILE).exists());
        assert!(!root.join(MARKER_FILE).exists());
        // The one file that was there is untouched.
        assert_eq!(std::fs::read_to_string(root.join("params.toml")).unwrap(), "MOJE");
        // Exactly one entry in the folder: the user's own.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn init_refuses_a_folder_that_is_already_a_workspace() {
        let td = tempfile::tempdir().unwrap();
        let prof = td.path().join("profile");
        std::fs::create_dir(&prof).unwrap();
        let from = fake_profile(&prof);
        let root = td.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        write_marker(&root).unwrap();

        assert_eq!(
            init_workspace(&root, &from).unwrap_err().message,
            "složka už je pracovní prostor"
        );
        assert!(!root.join("vault.bin").exists());
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
             # Dočasné soubory atomických zápisů (po pádu aplikace mohou zůstat).\n\
             # Aplikace je vždy pojmenuje <soubor>.tmp, proto jediné pravidlo:\n\
             *.tmp\n\
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
