//! Shared filesystem rails for every writer into a USER-CHOSEN folder.
//!
//! ONE implementation of: single-component path validation, the
//! Unicode-aware case-insensitive existence probe, and the atomic
//! tmp + `sync_all` + rename write. The scripts library (`dbc-ui`) and
//! the workspace folder (`workspace.rs`) both call these — the scripts
//! T2 review's WIDER-SCOPE note is binding: a second "quick" probe or a
//! second tmp+rename is exactly how the ASCII-only comparison and the
//! empty-component hole got written in the first place.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::StateError;

fn err(m: impl Into<String>) -> StateError {
    StateError { message: m.into() }
}

/// Validates ONE path component and joins it onto `base`.
///
/// Rejects empty, `.`, `..`, anything containing `/`, `\` or `:`, and any
/// control character. Empty is rejected DELIBERATELY (scripts T2 review):
/// an empty component silently resolving to `base` turns "create X here"
/// into "operate on the root itself".
pub fn join_component(base: &Path, component: &str) -> Result<PathBuf, StateError> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains(':')
        || component.chars().any(char::is_control)
    {
        return Err(err("neplatná cesta"));
    }
    Ok(base.join(component))
}

/// Case-INSENSITIVE collision probe inside ONE directory — the single
/// `read_dir` loop this workspace is allowed to have for the purpose.
/// Returns the EXACT on-disk name that would collide with `name`.
///
/// Case folding is UNICODE-aware and uses `str::to_uppercase`, NOT
/// `eq_ignore_ascii_case` (which misses `Č`/`č` and every other
/// non-ASCII pair) and NOT `str::to_lowercase`. The uppercase fold is a
/// deliberate T1-review decision, measured against NTFS rather than
/// assumed:
///
/// * `to_lowercase` implements the Unicode FINAL-SIGMA context rule, so
///   `ΟΔΟΣ` folds to `οδος` while a typed `οδοσ` stays `οδοσ` — the probe
///   reports "free" although NTFS resolves both to the SAME file. That is
///   a false NEGATIVE, i.e. the silent-overwrite direction.
/// * `to_uppercase` has no contextual rules, so it tracks the NTFS
///   `$UpCase` table directly. Every pair where it folds LESS than
///   lowercasing (`ß`/`ẞ`, `K`/`K` U+212A, `Å`/`å` U+212B, `µ`/`μ`,
///   `İ`/`i`, `ǅ`/`ǆ`) was measured to be a DISTINCT file on NTFS, so
///   nothing is lost; where it folds MORE (final `ς` → `Σ`, `ﬁ` → `FI`)
///   the result is a refused name, which is the safe direction.
///
/// `ignore_exact` is the byte-exact name of the entry being renamed, so
/// an entry never collides with ITSELF (identity, not name equality — on
/// a case-SENSITIVE volume a coexisting `A.SQL` must still block
/// `a.sql` → `A.SQL`).
///
/// Non-UTF-8 names are compared lossily: a false positive merely refuses
/// a name. An unreadable directory is an `Err`, never a silent "free".
pub fn conflicting_entry_ci(
    parent: &Path,
    name: &str,
    ignore_exact: Option<&str>,
) -> Result<Option<String>, StateError> {
    let rd = fs::read_dir(parent)
        .map_err(|e| err(format!("nelze číst složku {}: {e}", parent.display())))?;
    let want = name.to_uppercase();
    for ent in rd {
        let ent =
            ent.map_err(|e| err(format!("nelze číst složku {}: {e}", parent.display())))?;
        let existing = ent.file_name().to_string_lossy().into_owned();
        if ignore_exact.is_some_and(|ex| ex == existing.as_str()) {
            continue;
        }
        if existing.to_uppercase() == want {
            return Ok(Some(existing));
        }
    }
    Ok(None)
}

/// Case-INSENSITIVE existence probe inside ONE directory — the boolean
/// face of [`conflicting_entry_ci`] (same loop, same fold, no second
/// `read_dir`). An unreadable directory is an `Err`, never a silent
/// `false`.
pub fn entry_exists_ci(parent: &Path, name: &str) -> Result<bool, StateError> {
    Ok(conflicting_entry_ci(parent, name, None)?.is_some())
}

/// Atomic write: `<path>.tmp` + `sync_all` + rename (the
/// `AppConfig::save` shape). Takes BYTES, so it carries `vault.bin` as
/// happily as a `.sql` file. The tmp file is removed on every failure
/// path. Does NOT create parent directories — callers own their folder.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    use std::io::Write as _;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = write {
        let _ = fs::remove_file(&tmp);
        return Err(err(format!("uložení selhalo: {e}")));
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err(format!("uložení selhalo: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_component_refuses_empty_dot_dotdot_and_separators() {
        let base = Path::new("D:\\ws");
        for bad in ["", ".", "..", "a/b", "a\\b", "C:", "a\u{0}b"] {
            assert!(join_component(base, bad).is_err(), "must refuse {bad:?}");
        }
        assert_eq!(join_component(base, "config.toml").unwrap(), base.join("config.toml"));
    }

    #[test]
    fn entry_exists_ci_is_unicode_aware_not_ascii_only() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("Čtení.sql"), b"").unwrap();
        // ASCII-insensitive compare misses this pair; Unicode case folding does not.
        assert!(entry_exists_ci(td.path(), "čtení.sql").unwrap());
        assert!(entry_exists_ci(td.path(), "ČTENÍ.SQL").unwrap());
        assert!(!entry_exists_ci(td.path(), "jine.sql").unwrap());
    }

    #[test]
    fn entry_exists_ci_detects_the_greek_sigma_folder_pair() {
        // REGRESSION PIN (T1 review follow-up (a)). `str::to_lowercase`
        // implements the Unicode FINAL-SIGMA context rule: "ΟΔΟΣ" lowercases
        // to "οδος" (final ς) while the typed "οδοσ" stays "οδοσ", so a
        // lowercase fold reports "free" — yet NTFS says SAME FILE (measured:
        // creating "ΟΔΟΣ" then probing "οδοσ" resolves to the same entry).
        // That is a FALSE NEGATIVE, i.e. the data-loss direction. Uppercasing
        // has no contextual rule, so it tracks the $UpCase table directly.
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir(td.path().join("ΟΔΟΣ")).unwrap();
        assert!(
            entry_exists_ci(td.path(), "οδοσ").unwrap(),
            "Σ/σ fold apart under to_lowercase but are ONE file on NTFS"
        );
        // The final-sigma spelling is a different file on NTFS; refusing it
        // too is an over-approximation, which is the SAFE direction.
        assert!(entry_exists_ci(td.path(), "οδος").unwrap());
    }

    #[test]
    fn conflicting_entry_ci_reports_the_exact_name_and_skips_the_identity() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("Řezy.sql"), b"").unwrap();
        assert_eq!(
            conflicting_entry_ci(td.path(), "řezy.sql", None).unwrap(),
            Some("Řezy.sql".to_string())
        );
        // An entry never collides with ITSELF (exact-name identity).
        assert_eq!(conflicting_entry_ci(td.path(), "ŘEZY.sql", Some("Řezy.sql")).unwrap(), None);
        assert_eq!(conflicting_entry_ci(td.path(), "jine.sql", None).unwrap(), None);
    }

    #[test]
    fn entry_exists_ci_on_an_unreadable_dir_is_err_not_false() {
        let td = tempfile::tempdir().unwrap();
        let gone = td.path().join("neni");
        assert!(entry_exists_ci(&gone, "a.sql").is_err());
    }

    #[test]
    fn write_atomic_leaves_no_tmp_file_and_writes_bytes() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("vault.bin");
        write_atomic(&p, &[0u8, 1, 2, 255]).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), vec![0u8, 1, 2, 255]);
        let leftovers: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp left behind: {leftovers:?}");
    }

    #[test]
    fn write_atomic_overwrites_in_place() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("config.toml");
        write_atomic(&p, b"a = 1").unwrap();
        write_atomic(&p, b"a = 2").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a = 2");
    }

    #[test]
    fn write_atomic_failure_leaves_no_tmp_behind() {
        let td = tempfile::tempdir().unwrap();
        // A directory cannot be replaced by a file rename, so the rename arm
        // fails deterministically — the tmp sibling must still be cleaned up.
        let p = td.path().join("busy");
        std::fs::create_dir(&p).unwrap();
        assert!(write_atomic(&p, b"x").is_err());
        assert!(!td.path().join("busy.tmp").exists(), "tmp left behind after a failed rename");
    }
}
