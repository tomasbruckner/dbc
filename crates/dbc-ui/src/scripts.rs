//! Scripts library (Bruno model, design: scripts-library-design.md).
//! Pure model + std-fs helpers for the user-chosen `.sql` folder.
//! SECURITY (design §7): every path is built by joining VALIDATED single
//! components onto the root; symlinks are skipped at scan; nothing here
//! ever executes SQL or touches a database connection.
//!
//! Landed dark in scripts T2; lit incrementally. Workspace T7 lit the
//! first consumers (`scan_scripts`/`ScriptEntry`/`ScriptScan` drive the
//! „Skripty" sidebar section), T8 lit the editor binding's
//! `resolve_rel`/`read_script`/`write_script`/`SCRIPT_OPEN_CAP`, and
//! **T9 lit the last ten** — the fs mutations and their rails
//! (`resolve_entry_rel`, `create_script`, `create_folder`,
//! `rename_entry`, `delete_entry`, `validate_script_name`,
//! `conflicting_name`, `joined_rel`, `RESERVED_NAMES`,
//! `SCRIPT_NAME_CAP`). The module-level `#![allow(dead_code)]` that
//! carried them is GONE as of T9: nothing here is dark any more, so a
//! future unused item is a warning again rather than something this
//! attribute would have hidden.

use std::fs;
use std::path::{Path, PathBuf};

/// One entry of the scanned library, in DISPLAY order (depth-first;
/// within a directory: folders first, then files, each ordered by
/// case-insensitive name). `rel` uses '/' separators on all platforms —
/// stable expand keys and event payloads; `resolve_rel` maps back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptEntry {
    /// Root-relative path, '/'-joined on every platform.
    pub rel: String,
    /// Folder vs `.sql` file.
    pub is_dir: bool,
    /// 0 = direct child of the root.
    pub depth: usize,
}

/// A completed scan. `truncated`/`depth_clipped` drive the disclosure
/// Notice rows (2000-db precedent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptScan {
    /// Flat entry list already in render order.
    pub entries: Vec<ScriptEntry>,
    /// `SCRIPTS_ENTRY_CAP` was hit; the list is a prefix.
    pub truncated: bool,
    /// Some directory at `SCRIPTS_DEPTH_CAP` had eligible children that
    /// were not descended into.
    pub depth_clipped: bool,
}

/// Entry cap (design §7.5 — the 2000-database cap precedent, verbatim).
pub const SCRIPTS_ENTRY_CAP: usize = 2000;
/// Max nesting depth descended (policy cap; the walk is iterative, so
/// this is disclosure, not stack safety).
pub const SCRIPTS_DEPTH_CAP: usize = 12;
/// Editor-open size cap (design §7.6). Running via ▶ streams through the
/// G12 splitter and has NO such cap.
pub const SCRIPT_OPEN_CAP: u64 = 1_048_576;
/// Max characters of a created/renamed name (after `.sql` append).
pub const SCRIPT_NAME_CAP: usize = 80;

/// THE `.sql` test (design §1.5) — one rail, ASCII-case-insensitive
/// because an extension is not a user-facing name (`TRZBY.SQL` and
/// `trzby.sql` are both scripts, and no Unicode extension exists to fold).
///
/// T9 review NIT-3: this used to be private, and three call sites had each
/// re-spelled the same `extension().and_then(..).is_some_and(..)` chain —
/// the scan, the editor's save-as and the library's run. §6 says one rail,
/// so they all come here now.
pub fn is_sql_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

fn is_sql_name(name: &str) -> bool {
    is_sql_path(Path::new(name))
}

/// Lists ONE directory in display order (folders first, then `.sql`
/// files, case-insensitive name order). Skips symlinks entirely
/// (design §7.2), dot-directories (keeps `.git/` invisible AND
/// undescended, design §1.5) and non-UTF-8 names (cannot round-trip
/// through the '/'-joined rel convention).
///
/// Asymmetry, deliberate: only dot-DIRECTORIES are hidden here (a
/// dot-FILE ending in `.sql` is listed, since `.sql` files are the whole
/// point), while `validate_script_name` refuses to CREATE any dot-named
/// entry — the app shows what the user put there but never adds hidden
/// entries itself.
fn list_dir_sorted(dir: &Path, rel_prefix: &str, depth: usize) -> Result<Vec<ScriptEntry>, String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("nelze číst složku {}: {e}", dir.display()))?;
    let mut folders: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| format!("nelze číst složku {}: {e}", dir.display()))?;
        let Some(name) = ent.file_name().to_str().map(str::to_string) else { continue };
        let Ok(ft) = ent.file_type() else { continue };
        if ft.is_symlink() {
            continue; // design §7.2: never descended, never listed
        }
        if ft.is_dir() {
            if name.starts_with('.') {
                continue;
            }
            folders.push(name);
        } else if ft.is_file() && is_sql_name(&name) {
            files.push(name);
        }
    }
    // Display-only ordering, but it uses the SAME fold as the collision
    // probe (`fsutil::conflicting_entry_ci`, `to_uppercase`). Two names
    // the probe calls identical must not sort apart here, and one file
    // with two disagreeing folds is how the next reader learns the wrong
    // rule.
    folders.sort_by_key(|n| n.to_uppercase());
    files.sort_by_key(|n| n.to_uppercase());
    let make = |name: String, is_dir: bool| {
        let rel = if rel_prefix.is_empty() { name } else { format!("{rel_prefix}/{name}") };
        ScriptEntry { rel, is_dir, depth }
    };
    let mut out: Vec<ScriptEntry> = Vec::with_capacity(folders.len() + files.len());
    out.extend(folders.into_iter().map(|n| make(n, true)));
    out.extend(files.into_iter().map(|n| make(n, false)));
    Ok(out)
}

/// Scans the library ITERATIVELY (explicit LIFO stack — house rule: no
/// recursion). Children are pushed in reverse so they pop in display
/// order; a directory's subtree splices right after it, giving one flat
/// Vec already in render order.
pub fn scan_scripts(root: &Path) -> Result<ScriptScan, String> {
    if !root.is_dir() {
        return Err(format!("složka neexistuje: {}", root.display()));
    }
    let mut entries: Vec<ScriptEntry> = Vec::new();
    let mut truncated = false;
    let mut depth_clipped = false;
    let mut stack: Vec<(ScriptEntry, PathBuf)> = Vec::new();
    let seed = list_dir_sorted(root, "", 0)?;
    for e in seed.into_iter().rev() {
        let abs = root.join(e.rel.rsplit('/').next().unwrap_or(&e.rel));
        stack.push((e, abs));
    }
    while let Some((entry, abs)) = stack.pop() {
        if entries.len() >= SCRIPTS_ENTRY_CAP {
            truncated = true;
            break;
        }
        let is_dir = entry.is_dir;
        let depth = entry.depth;
        let rel = entry.rel.clone();
        entries.push(entry);
        if is_dir {
            if depth + 1 >= SCRIPTS_DEPTH_CAP {
                // Honest disclosure: only flag when the clipped dir
                // actually contains anything eligible.
                if let Ok(kids) = list_dir_sorted(&abs, &rel, depth + 1) {
                    if !kids.is_empty() {
                        depth_clipped = true;
                    }
                }
                continue;
            }
            let kids = list_dir_sorted(&abs, &rel, depth + 1)?;
            for k in kids.into_iter().rev() {
                let kabs = abs.join(k.rel.rsplit('/').next().unwrap_or(&k.rel));
                stack.push((k, kabs));
            }
        }
    }
    Ok(ScriptScan { entries, truncated, depth_clipped })
}

/// SECURITY (design §7.1): joins a '/'-separated rel onto the root. Each
/// component goes through the SHARED rail
/// `dbc_state::fsutil::join_component`, so empty/`.`/`..` components and
/// anything with `\`, `:` or control characters are refused in exactly
/// one place — no rel can ever resolve outside the root.
/// Defense in depth: rels only originate from `scan_scripts` (single
/// `file_name()` components) and `validate_script_name` output anyway.
///
/// The EMPTY rel is the root itself — that is what a top-level
/// `parent_rel` means. Every helper that MUTATES a resolved path must go
/// through `resolve_entry_rel` instead.
pub fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = root.to_path_buf();
    if rel.is_empty() {
        return Ok(p);
    }
    for comp in rel.split('/') {
        p = dbc_state::fsutil::join_component(&p, comp).map_err(|e| e.message)?;
    }
    Ok(p)
}

/// SECURITY: like `resolve_rel`, but REFUSES the empty rel. `resolve_rel`
/// deliberately maps `""` to the root itself (that is what a top-level
/// `parent_rel` means), so every helper that MUTATES a resolved path must
/// route through this rail instead — otherwise `delete_entry(root, "",
/// true)` removes the user's library folder and `rename_entry(root, "",
/// …)` renames it. The rail lives here, not in each caller, so future
/// writers over this root (config, vault, prefs) inherit it.
pub fn resolve_entry_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("kořen knihovny nelze měnit".to_string());
    }
    resolve_rel(root, rel)
}

/// Windows device names. The superscript COM¹/COM²/COM³ and LPT¹/LPT²/
/// LPT³ forms (U+00B9/U+00B2/U+00B3) are reserved by Win32 too — they
/// are NOT reachable by ASCII uppercasing, hence the explicit rows.
const RESERVED_NAMES: [&str; 28] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
    "LPT7", "LPT8", "LPT9", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}",
];

/// Validates a SINGLE name component typed in-app (design §7.3); returns
/// the effective name (files get `.sql` appended when missing). Czech
/// errors are display-ready.
pub fn validate_script_name(name: &str, is_file: bool) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("zadejte název".to_string());
    }
    let mut full = name.to_string();
    if is_file && !is_sql_name(&full) {
        full.push_str(".sql");
    }
    if full.chars().count() > SCRIPT_NAME_CAP {
        return Err(format!("název je příliš dlouhý (limit {SCRIPT_NAME_CAP} znaků)"));
    }
    if full
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control())
    {
        return Err("název obsahuje nepovolené znaky".to_string());
    }
    if full.starts_with('.') || full.ends_with('.') || full.starts_with(' ') || full.ends_with(' ')
    {
        return Err("název nesmí začínat ani končit tečkou nebo mezerou".to_string());
    }
    let stem = full.split('.').next().unwrap_or("");
    if RESERVED_NAMES.contains(&stem.to_ascii_uppercase().as_str()) {
        return Err("název je rezervovaný systémem".to_string());
    }
    Ok(full)
}

/// SECURITY: collision probe within one directory, returning the EXACT
/// on-disk name that would collide with `name`.
///
/// Thin adapter over the SHARED rail
/// `dbc_state::fsutil::conflicting_entry_ci` (workspace T1) — the fold
/// rule, the identity skip and the lossy non-UTF-8 comparison live there
/// so every writer into a user-chosen folder inherits the same
/// semantics. Why it must be Unicode-aware at all: NTFS is
/// case-insensitive across all of Unicode, so with ASCII-only folding
/// `Řezy.sql` and `řezy.sql` compare unequal here yet name the SAME file
/// on disk — the caller would see "no collision" and `write_script`'s
/// replace-rename would silently destroy the user's script. Czech names
/// make that a routine case.
fn conflicting_name(
    parent: &Path,
    name: &str,
    ignore_exact: Option<&str>,
) -> Result<Option<String>, String> {
    dbc_state::fsutil::conflicting_entry_ci(parent, name, ignore_exact).map_err(|e| e.message)
}

fn joined_rel(parent_rel: &str, name: &str) -> String {
    if parent_rel.is_empty() { name.to_string() } else { format!("{parent_rel}/{name}") }
}

/// Creates an EMPTY `.sql` file; returns its new rel. Never overwrites —
/// the Unicode-aware `conflicting_name` probe is what makes that true on
/// a case-insensitive filesystem (`write_script` itself REPLACES).
pub fn create_script(root: &Path, parent_rel: &str, name: &str) -> Result<String, String> {
    let file = validate_script_name(name, true)?;
    let parent = resolve_rel(root, parent_rel)?;
    if conflicting_name(&parent, &file, None)?.is_some() {
        return Err("název už existuje".to_string());
    }
    write_script(&parent.join(&file), "")?;
    Ok(joined_rel(parent_rel, &file))
}

/// Creates a folder; returns its new rel. Never overwrites.
pub fn create_folder(root: &Path, parent_rel: &str, name: &str) -> Result<String, String> {
    let folder = validate_script_name(name, false)?;
    let parent = resolve_rel(root, parent_rel)?;
    if conflicting_name(&parent, &folder, None)?.is_some() {
        return Err("název už existuje".to_string());
    }
    fs::create_dir(parent.join(&folder)).map_err(|e| format!("vytvoření složky selhalo: {e}"))?;
    Ok(joined_rel(parent_rel, &folder))
}

/// Renames within the same directory; returns the new rel. A case-only
/// rename is allowed because the probe skips the entry ITSELF by exact
/// name — not by name equality, so a coexisting differently-cased
/// sibling on a case-sensitive directory still blocks the rename.
pub fn rename_entry(root: &Path, rel: &str, new_name: &str, is_dir: bool) -> Result<String, String> {
    let new_file = validate_script_name(new_name, !is_dir)?;
    let old = resolve_entry_rel(root, rel)?;
    let (parent_rel, old_name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let parent = resolve_rel(root, parent_rel)?;
    if conflicting_name(&parent, &new_file, Some(old_name))?.is_some() {
        return Err("název už existuje".to_string());
    }
    fs::rename(&old, parent.join(&new_file)).map_err(|e| format!("přejmenování selhalo: {e}"))?;
    Ok(joined_rel(parent_rel, &new_file))
}

/// Deletes a file, or an EMPTY folder (design §7.9 — no recursive delete
/// in v1: git can restore a deleted file; we cannot). The root itself is
/// never deletable (`resolve_entry_rel`).
pub fn delete_entry(root: &Path, rel: &str, is_dir: bool) -> Result<(), String> {
    let p = resolve_entry_rel(root, rel)?;
    if is_dir {
        let mut rd =
            fs::read_dir(&p).map_err(|e| format!("nelze číst složku {}: {e}", p.display()))?;
        if rd.next().is_some() {
            return Err("složka není prázdná — smažte nejdřív její obsah".to_string());
        }
        fs::remove_dir(&p).map_err(|e| format!("smazání selhalo: {e}"))
    } else {
        fs::remove_file(&p).map_err(|e| format!("smazání selhalo: {e}"))
    }
}

/// Atomic write via the SHARED rail `dbc_state::fsutil::write_atomic`
/// (`.tmp` sibling + `sync_all` + rename, the `AppConfig::save` shape).
/// REPLACES an existing target by design (that is what saving an open
/// editor buffer means) — callers that must not clobber, like
/// `create_script`, are responsible for probing via `conflicting_name`
/// first. Last-writer-wins on external edits — by the user's own model
/// git is the history layer (design §5.2).
pub fn write_script(path: &Path, text: &str) -> Result<(), String> {
    dbc_state::fsutil::write_atomic(path, text.as_bytes()).map_err(|e| e.message)
}

/// Reads a script for the EDITOR: refuses symlinks, > `SCRIPT_OPEN_CAP`,
/// and non-UTF-8 (an error, not a lossy mangle — design §5.1).
pub fn read_script(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("soubor nelze otevřít: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("symbolické odkazy nejsou podporovány".to_string());
    }
    if meta.len() > SCRIPT_OPEN_CAP {
        return Err(
            "soubor je příliš velký pro editor (limit 1 MiB) — spusťte jej jako skript".to_string(),
        );
    }
    let bytes = fs::read(path).map_err(|e| format!("soubor nelze otevřít: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "soubor není platné UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        fs::write(p, b"select 1;").unwrap();
    }

    #[test]
    fn scan_orders_folders_first_then_files_case_insensitive() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        touch(&r.join("b.sql"));
        touch(&r.join("A.sql"));
        fs::create_dir(r.join("zeta")).unwrap();
        fs::create_dir(r.join("Alpha")).unwrap();
        touch(&r.join("Alpha").join("inner.sql"));
        let scan = scan_scripts(r).unwrap();
        let rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, vec!["Alpha", "Alpha/inner.sql", "zeta", "A.sql", "b.sql"]);
        assert_eq!(scan.entries[1].depth, 1);
        assert!(!scan.truncated && !scan.depth_clipped);
    }

    #[test]
    fn scan_shows_only_sql_and_skips_dot_dirs() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        // Plan deviation (NTFS): the plan's fixture wrote `q.sql` AND
        // `q.SQL`, which are ONE file on a case-insensitive filesystem —
        // distinct stems keep the uppercase-extension coverage honest.
        touch(&r.join("q.sql"));
        touch(&r.join("r.SQL"));
        fs::write(r.join("readme.md"), b"x").unwrap();
        fs::write(r.join("noext"), b"x").unwrap();
        fs::create_dir(r.join(".git")).unwrap();
        touch(&r.join(".git").join("hidden.sql"));
        let scan = scan_scripts(r).unwrap();
        let rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, vec!["q.sql", "r.SQL"]);
    }

    #[test]
    fn scan_missing_root_is_czech_error() {
        let td = tempfile::tempdir().unwrap();
        let gone = td.path().join("neni");
        let err = scan_scripts(&gone).unwrap_err();
        assert!(err.starts_with("složka neexistuje:"), "{err}");
    }

    #[test]
    fn scan_entry_cap_sets_truncated() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        for i in 0..(SCRIPTS_ENTRY_CAP + 1) {
            fs::write(r.join(format!("f{i:04}.sql")), b"").unwrap();
        }
        let scan = scan_scripts(r).unwrap();
        assert_eq!(scan.entries.len(), SCRIPTS_ENTRY_CAP);
        assert!(scan.truncated);
    }

    #[test]
    fn scan_depth_cap_sets_depth_clipped() {
        let td = tempfile::tempdir().unwrap();
        let mut p = td.path().to_path_buf();
        for i in 0..(SCRIPTS_DEPTH_CAP + 1) {
            p.push(format!("d{i}"));
            fs::create_dir(&p).unwrap();
        }
        touch(&p.join("deep.sql"));
        let scan = scan_scripts(td.path()).unwrap();
        assert!(scan.depth_clipped);
        assert!(scan.entries.iter().all(|e| e.depth < SCRIPTS_DEPTH_CAP));
        assert!(!scan.entries.iter().any(|e| e.rel.ends_with("deep.sql")));
    }

    #[test]
    fn scan_skips_symlinks_when_creatable() {
        // Windows symlink creation needs privilege (developer mode) —
        // skip silently when unavailable rather than failing the suite.
        let td = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        touch(&outside.path().join("secret.sql"));
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(outside.path(), td.path().join("link")).is_ok();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.path(), td.path().join("link")).is_ok();
        if !made {
            eprintln!(
                "SKIP scan_skips_symlinks_when_creatable: symlink creation unavailable \
                 (needs developer mode / privilege) — the skip rail was NOT verified here"
            );
            return;
        }
        let scan = scan_scripts(td.path()).unwrap();
        assert!(scan.entries.is_empty(), "symlinked dir must be invisible: {:?}", scan.entries);
    }

    /// The symlink test above SKIPS on a stock Windows box (creating a
    /// symlink needs privilege), which would leave the no-escape property
    /// unverified there. A directory JUNCTION needs no privilege and is
    /// the reparse point an attacker/user can actually plant, so this
    /// pins the OBSERVABLE property — a junction is never listed and never
    /// descended — on an unprivileged machine.
    ///
    /// HONESTY NOTE (mutation-tested, T1 review): this test does NOT pin
    /// the `if ft.is_symlink() { continue }` line. Deleting that line
    /// leaves the test GREEN, because a junction reports
    /// `is_symlink=true, is_dir=false, is_file=false` and therefore falls
    /// through both classification arms anyway. What the test DOES catch
    /// is a swap of the `file_type()` classification to a link-FOLLOWING
    /// `fs::metadata()`, which would make the junction look like a plain
    /// directory and hand the walk a path out of the root. The
    /// `is_symlink` skip is pinned by `scan_skips_symlinks_when_creatable`
    /// — on a machine where symlink creation is available.
    #[cfg(windows)]
    #[test]
    fn scan_skips_directory_junctions() {
        let td = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        touch(&outside.path().join("secret.sql"));
        let link = td.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("SKIP scan_skips_directory_junctions: mklink /J unavailable");
            return;
        }
        let scan = scan_scripts(td.path()).unwrap();
        assert!(
            scan.entries.is_empty(),
            "junction must never be listed or descended (root escape): {:?}",
            scan.entries
        );
    }

    #[test]
    fn resolve_rel_rejects_traversal_shapes() {
        let root = Path::new("D:/lib");
        // "a:b" pins the ADS/drive-colon rail INDEPENDENTLY: "C:\\x" also
        // contains a backslash, so it would stay rejected even if the ':'
        // check were deleted.
        for bad in [
            "..",
            "a/../b",
            "a/..",
            "/abs",
            "a//b",
            "C:\\x",
            "a:b",
            "\\\\srv\\share",
            "a\u{0}b",
            ".",
        ] {
            assert!(resolve_rel(root, bad).is_err(), "{bad} must be rejected");
        }
        assert_eq!(resolve_rel(root, "a/b.sql").unwrap(), root.join("a").join("b.sql"));
        assert_eq!(resolve_rel(root, "").unwrap(), root.to_path_buf());
    }

    #[test]
    fn validate_script_name_table() {
        assert_eq!(validate_script_name("dotaz", true).unwrap(), "dotaz.sql");
        assert_eq!(validate_script_name("dotaz.SQL", true).unwrap(), "dotaz.SQL");
        assert_eq!(validate_script_name("slozka", false).unwrap(), "slozka");
        assert!(validate_script_name("", true).is_err());
        assert!(validate_script_name("   ", true).is_err());
        assert!(validate_script_name("a/b", true).is_err());
        assert!(validate_script_name("a\\b", true).is_err());
        assert!(validate_script_name("a:b", true).is_err());
        assert!(validate_script_name("a?b", true).is_err());
        assert!(validate_script_name(".skryty", false).is_err());
        assert!(validate_script_name("konec.", false).is_err());
        assert!(validate_script_name("con", true).is_err());
        assert!(validate_script_name("LPT3", false).is_err());
        assert!(validate_script_name("x\u{7}y", true).is_err());
        let long = "x".repeat(SCRIPT_NAME_CAP);
        assert!(validate_script_name(&long, true).is_err()); // + ".sql" overflows
    }

    #[test]
    fn create_rename_delete_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        let f = create_folder(r, "", "reporty").unwrap();
        assert_eq!(f, "reporty");
        let s = create_script(r, "reporty", "prodeje").unwrap();
        assert_eq!(s, "reporty/prodeje.sql");
        assert!(r.join("reporty").join("prodeje.sql").is_file());
        assert_eq!(create_script(r, "reporty", "PRODEJE").unwrap_err(), "název už existuje");
        let s2 = rename_entry(r, "reporty/prodeje.sql", "nakupy", false).unwrap();
        assert_eq!(s2, "reporty/nakupy.sql");
        assert!(!r.join("reporty").join("prodeje.sql").exists());
        assert_eq!(
            delete_entry(r, "reporty", true).unwrap_err(),
            "složka není prázdná — smažte nejdřív její obsah"
        );
        delete_entry(r, "reporty/nakupy.sql", false).unwrap();
        delete_entry(r, "reporty", true).unwrap();
        assert!(!r.join("reporty").exists());
    }

    #[test]
    fn write_and_read_script_roundtrip_and_caps() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("a.sql");
        write_script(&p, "select 42;\n").unwrap();
        assert_eq!(read_script(&p).unwrap(), "select 42;\n");
        assert!(!td.path().join("a.sql.tmp").exists());
        let big = td.path().join("big.sql");
        fs::write(&big, vec![b' '; (SCRIPT_OPEN_CAP + 1) as usize]).unwrap();
        assert!(read_script(&big).unwrap_err().contains("příliš velký"));
        let bad = td.path().join("bad.sql");
        fs::write(&bad, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        assert_eq!(read_script(&bad).unwrap_err(), "soubor není platné UTF-8");
    }

    #[test]
    fn write_script_replaces_an_existing_target() {
        // Load-bearing property: saving an open buffer overwrites. It is
        // exactly WHY create_* must probe for collisions first.
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("a.sql");
        write_script(&p, "first").unwrap();
        write_script(&p, "second").unwrap();
        assert_eq!(read_script(&p).unwrap(), "second");
    }

    #[test]
    fn create_refuses_unicode_case_variant_of_existing_name() {
        // MAJOR regression (Czech UI, NTFS): `Ř` vs `ř` fold equal on
        // disk but NOT under eq_ignore_ascii_case — with ASCII folding
        // the probe said "free", write_script replaced the rename target
        // and the user's script was destroyed silently.
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        fs::write(r.join("Řezy.sql"), b"-- puvodni obsah").unwrap();
        assert_eq!(create_script(r, "", "řezy").unwrap_err(), "název už existuje");
        assert_eq!(
            fs::read_to_string(r.join("Řezy.sql")).unwrap(),
            "-- puvodni obsah",
            "the original file must be untouched"
        );
        // Non-Czech accented pair, folders too.
        create_folder(r, "", "Ärchiv").unwrap();
        assert_eq!(create_folder(r, "", "ärchiv").unwrap_err(), "název už existuje");
    }

    #[test]
    fn rename_refuses_unicode_case_variant_but_allows_case_only_self_rename() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        fs::write(r.join("Řezy.sql"), b"-- puvodni obsah").unwrap();
        fs::write(r.join("jine.sql"), b"-- jine").unwrap();
        assert_eq!(
            rename_entry(r, "jine.sql", "řezy", false).unwrap_err(),
            "název už existuje"
        );
        assert_eq!(fs::read_to_string(r.join("Řezy.sql")).unwrap(), "-- puvodni obsah");
        // Identity: an entry never collides with ITSELF (exact-name skip),
        // so a case-only rename still works — and so does a no-op rename.
        assert_eq!(rename_entry(r, "jine.sql", "JINE", false).unwrap(), "JINE.sql");
        assert_eq!(rename_entry(r, "JINE.sql", "JINE", false).unwrap(), "JINE.sql");
        assert_eq!(fs::read_to_string(r.join("JINE.sql")).unwrap(), "-- jine");
    }

    #[test]
    fn empty_rel_can_never_mutate_the_library_root() {
        // MINOR 2: resolve_rel("") is the ROOT by design (parent_rel), so
        // the mutation helpers must refuse it or they delete/rename the
        // user's whole library folder.
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        touch(&r.join("a.sql"));
        assert_eq!(delete_entry(r, "", true).unwrap_err(), "kořen knihovny nelze měnit");
        assert_eq!(delete_entry(r, "   ", true).unwrap_err(), "kořen knihovny nelze měnit");
        assert_eq!(
            rename_entry(r, "", "jinak", true).unwrap_err(),
            "kořen knihovny nelze měnit"
        );
        assert!(r.is_dir(), "root must survive");
        assert!(r.join("a.sql").is_file());
        assert!(resolve_entry_rel(r, "").is_err());
        assert_eq!(resolve_entry_rel(r, "a.sql").unwrap(), r.join("a.sql"));
        // The actual data-loss shape: an EMPTY root passes the
        // "folder not empty" guard, so only the rail stops the removal.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            delete_entry(empty.path(), "", true).unwrap_err(),
            "kořen knihovny nelze měnit"
        );
        assert!(empty.path().is_dir(), "empty root must survive");
    }

    #[test]
    fn superscript_device_names_are_reserved() {
        assert!(validate_script_name("COM\u{b9}", true).is_err());
        assert!(validate_script_name("lpt\u{b2}", false).is_err());
        assert!(validate_script_name("com\u{b3}", true).is_err());
    }
}
