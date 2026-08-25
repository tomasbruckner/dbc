//! Scripts library (Bruno model, design: scripts-library-design.md).
//! Pure model + std-fs helpers for the user-chosen `.sql` folder.
//! SECURITY (design §7): every path is built by joining VALIDATED single
//! components onto the root; symlinks are skipped at scan; nothing here
//! ever executes SQL or touches a database connection.
//!
//! Landed dark in scripts T2 (unit-tested, unreachable from `main` until
//! T3+ wire the sidebar section and fs dispatch). Removal owner for this
//! allow: T3/T4 (scan + state consumers), T5 (read/write), T6 (mutations)
//! — drop it the moment the first consumer lands (admin_sql.rs precedent).
#![allow(dead_code)]

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

fn is_sql_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

/// Lists ONE directory in display order (folders first, then `.sql`
/// files, case-insensitive name order). Skips symlinks entirely
/// (design §7.2), dot-directories (keeps `.git/` invisible AND
/// undescended, design §1.5) and non-UTF-8 names (cannot round-trip
/// through the '/'-joined rel convention).
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
    folders.sort_by_key(|n| n.to_lowercase());
    files.sort_by_key(|n| n.to_lowercase());
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

/// SECURITY (design §7.1): joins a '/'-separated rel onto the root,
/// rejecting empty/`.`/`..` components and anything with `\`, `:` or
/// control characters — no rel can ever resolve outside the root.
/// Defense in depth: rels only originate from `scan_scripts` (single
/// `file_name()` components) and `validate_script_name` output anyway.
pub fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = root.to_path_buf();
    if rel.is_empty() {
        return Ok(p);
    }
    for comp in rel.split('/') {
        if comp.is_empty()
            || comp == "."
            || comp == ".."
            || comp.contains('\\')
            || comp.contains(':')
            || comp.chars().any(|c| c.is_control())
        {
            return Err("neplatná cesta".to_string());
        }
        p.push(comp);
    }
    Ok(p)
}

const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
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

/// Case-insensitive existence probe within one directory (Windows-honest
/// collision check; ASCII-insensitive matches NTFS's common case).
fn entry_exists(parent: &Path, name: &str) -> Result<bool, String> {
    let rd =
        fs::read_dir(parent).map_err(|e| format!("nelze číst složku {}: {e}", parent.display()))?;
    for ent in rd {
        let ent =
            ent.map_err(|e| format!("nelze číst složku {}: {e}", parent.display()))?;
        if ent.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn joined_rel(parent_rel: &str, name: &str) -> String {
    if parent_rel.is_empty() { name.to_string() } else { format!("{parent_rel}/{name}") }
}

/// Creates an EMPTY `.sql` file; returns its new rel. Never overwrites.
pub fn create_script(root: &Path, parent_rel: &str, name: &str) -> Result<String, String> {
    let file = validate_script_name(name, true)?;
    let parent = resolve_rel(root, parent_rel)?;
    if entry_exists(&parent, &file)? {
        return Err("název už existuje".to_string());
    }
    write_script(&parent.join(&file), "")?;
    Ok(joined_rel(parent_rel, &file))
}

/// Creates a folder; returns its new rel.
pub fn create_folder(root: &Path, parent_rel: &str, name: &str) -> Result<String, String> {
    let folder = validate_script_name(name, false)?;
    let parent = resolve_rel(root, parent_rel)?;
    if entry_exists(&parent, &folder)? {
        return Err("název už existuje".to_string());
    }
    fs::create_dir(parent.join(&folder)).map_err(|e| format!("vytvoření složky selhalo: {e}"))?;
    Ok(joined_rel(parent_rel, &folder))
}

/// Renames within the same directory; returns the new rel. A case-only
/// rename of the same entry is allowed (the collision probe would
/// false-positive on itself).
pub fn rename_entry(root: &Path, rel: &str, new_name: &str, is_dir: bool) -> Result<String, String> {
    let new_file = validate_script_name(new_name, !is_dir)?;
    let old = resolve_rel(root, rel)?;
    let (parent_rel, old_name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let parent = resolve_rel(root, parent_rel)?;
    if !old_name.eq_ignore_ascii_case(&new_file) && entry_exists(&parent, &new_file)? {
        return Err("název už existuje".to_string());
    }
    fs::rename(&old, parent.join(&new_file)).map_err(|e| format!("přejmenování selhalo: {e}"))?;
    Ok(joined_rel(parent_rel, &new_file))
}

/// Deletes a file, or an EMPTY folder (design §7.9 — no recursive delete
/// in v1: git can restore a deleted file; we cannot).
pub fn delete_entry(root: &Path, rel: &str, is_dir: bool) -> Result<(), String> {
    let p = resolve_rel(root, rel)?;
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

/// Atomic write: `.tmp` sibling + `sync_all` + rename (`AppConfig::save`
/// shape). Last-writer-wins on external edits — by the user's own model
/// git is the history layer (design §5.2).
pub fn write_script(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = write {
        let _ = fs::remove_file(&tmp);
        return Err(format!("uložení selhalo: {e}"));
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("uložení selhalo: {e}")
    })
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
            return;
        }
        let scan = scan_scripts(td.path()).unwrap();
        assert!(scan.entries.is_empty(), "symlinked dir must be invisible: {:?}", scan.entries);
    }

    #[test]
    fn resolve_rel_rejects_traversal_shapes() {
        let root = Path::new("D:/lib");
        for bad in ["..", "a/../b", "a/..", "/abs", "a//b", "C:\\x", "\\\\srv\\share", "a\u{0}b", "."]
        {
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
}
