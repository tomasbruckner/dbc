//! Shared filesystem rails for every writer into a USER-CHOSEN folder.
//!
//! ONE implementation of: single-component path validation, the
//! Unicode-aware case-insensitive existence probe, and the atomic
//! tmp + `sync_all` + rename write. The scripts library (`dbc-ui`) and
//! the workspace folder (`workspace.rs`) both call these — the scripts
//! T2 review's WIDER-SCOPE note is binding: a second "quick" probe or a
//! second tmp+rename is exactly how the ASCII-only comparison and the
//! empty-component hole got written in the first place.
//!
//! SCOPE, stated honestly: this is the single rail for writes into a
//! folder the USER picked — where a name collision is someone's data and
//! a partial write is visible in their file manager (and, in a workspace,
//! in `git status`). It is NOT yet the only atomic writer in the repo:
//! `config.rs`, `params.rs`, `view_prefs.rs`, `vault.rs` and
//! `grid.rs` each still roll their own tmp + `sync_all` + rename into the
//! PROFILE dir (or an export target). That is a separate, pre-existing
//! population, deliberately left unswept by this phase; anything NEW
//! belongs here.

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

/// THE case fold for on-disk names — ONE implementation, workspace-wide.
///
/// T10 carry-forward 6. Three places in two crates were folding names for
/// the same purpose ("would NTFS call these one file?") and they did not
/// agree: this probe and `scripts::list_dir_sorted`'s ordering key used
/// `to_uppercase`, while Task 9's `dbc_ui::path_fold` (the editor
/// binding's affected-by-a-mutation test) used `to_lowercase` — and its
/// own doc comment claimed to be applying "the SAME rule
/// `dbc_state::fsutil` applies to names", naming `to_lowercase`, which was
/// simply false. The disagreement is not cosmetic: `to_lowercase`'s
/// final-sigma FALSE NEGATIVE (below) meant a delete of `…/ΟΔΟΣ.sql` did
/// not clear a binding on `…/οδοσ.sql`, although NTFS resolves both to one
/// file — the caption would go on naming a file that is gone and the next
/// Ctrl+S would silently recreate it, which is the exact bug `path_fold`
/// was introduced to prevent. Unified onto the measured fold, and made a
/// named function so a fourth site cannot quietly pick the other one.
///
/// Callers: [`conflicting_entry_ci`] (this file), `dbc_ui::path_fold`,
/// `dbc_ui::scripts::list_dir_sorted`.
///
/// Case folding is UNICODE-aware and uses `str::to_uppercase`, NOT
/// `eq_ignore_ascii_case` and NOT `str::to_lowercase`, for the reasons
/// measured against NTFS on [`conflicting_entry_ci`].
pub fn fold_name(name: &str) -> String {
    name.to_uppercase()
}

/// Case-INSENSITIVE collision probe inside ONE directory — the single
/// `read_dir` loop this workspace is allowed to have for the purpose.
/// Returns the EXACT on-disk name that would collide with `name`.
///
/// Case folding is UNICODE-aware and uses [`fold_name`], i.e.
/// `str::to_uppercase`, NOT
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
///   `$UpCase` table directly. The pairs where it folds LESS than
///   lowercasing are exactly three — `ß`/`ẞ`, `K`/`K` (U+212A KELVIN
///   SIGN), `Å`/`å` (U+212B ANGSTROM SIGN) — and all three were measured
///   to be DISTINCT files on NTFS, so nothing is lost. Where it folds
///   MORE (final `ς` → `Σ`, `ﬁ` → `FI`, `µ` U+00B5 / `μ` U+03BC) the
///   result is at worst a refused name, which is the safe direction.
///   Two further pairs that look like they belong on one of those lists
///   belong on NEITHER: `İ` U+0130 / `i` folds APART under both folds,
///   and `ǅ` U+01C5 / `ǆ` U+01C6 folds TOGETHER under both. (An earlier
///   revision of this comment listed `µ`/`μ`, `İ`/`i` and `ǅ`/`ǆ` as
///   "folds less"; that was wrong in all three cases. The decision is
///   unaffected — all six are distinct files on NTFS — but the
///   justification is what the next person re-derives from, so it is
///   pinned by `uppercase_fold_facts_this_doc_relies_on` below.)
///
/// Independently re-verified against the real `$UpCase` relation rather
/// than against the Unicode tables: enumerating 62,474 BMP names on NTFS
/// yields 973 colliding pairs. Of those, `to_uppercase` misses 0,
/// `to_lowercase` also misses 0 by count but carries the final-sigma
/// FALSE NEGATIVE above on a pair NTFS unifies, and
/// `eq_ignore_ascii_case` misses 947.
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
    let want = fold_name(name);
    for ent in rd {
        let ent =
            ent.map_err(|e| err(format!("nelze číst složku {}: {e}", parent.display())))?;
        let existing = ent.file_name().to_string_lossy().into_owned();
        if ignore_exact.is_some_and(|ex| ex == existing.as_str()) {
            continue;
        }
        if fold_name(&existing) == want {
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

/// The scratch file [`write_atomic`] writes before renaming: `<path>.tmp`,
/// nothing more. Public so the single-writer contract documented on
/// `write_atomic` is a testable property rather than a promise — it is
/// exactly BECAUSE this is a pure function of `path` that two overlapping
/// writes to one target collide. Keep the `.tmp` ending: §W6.2's shipped
/// `.gitignore` matches on it.
///
/// SCOPE: this is now the ONLY place the `.tmp` naming rule lives.
/// `config.rs`, `params.rs`, `view_prefs.rs`, `vault.rs`, `schema_cache.rs`
/// and `dbc-ui`'s `grid.rs` still each own their tmp+rename sequence — they
/// stream, sync and report errors differently enough that folding them into
/// `write_atomic` would cost more than it buys — but they all derive the
/// scratch name from here. `tmp_naming_has_a_single_owner` (below) fails the
/// build if a seventh site starts spelling it out again. Each of them
/// previously wrote `with_extension("toml.tmp")` / `push(".tmp")`, which
/// agrees with this function for every path they actually use; the point of
/// the move is that a future change to the rule reaches all of them.
pub fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Atomic write: `<path>.tmp` + `sync_all` + rename (the
/// `AppConfig::save` shape). Takes BYTES, so it carries `vault.bin` as
/// happily as a `.sql` file. The tmp file is removed on every failure
/// path. Does NOT create parent directories — callers own their folder.
///
/// **SINGLE WRITER PER PATH — a caller contract, not an implementation
/// detail** (workspace T8 review MAJOR-2). The tmp path is a DETERMINISTIC
/// function of `path` (see [`tmp_path_for`]), and `sync_all` holds it open
/// for tens of milliseconds. Two overlapping calls for the same target
/// therefore share one tmp file and race three ways: the second
/// `File::create` truncates the first's tmp mid-`write_all` (a
/// byte-interleaved result file), the loser's `rename` fails ENOENT and
/// reports „uložení selhalo" over a file that is perfectly fine, and — the
/// dangerous one — the winner on disk need not be the caller whose
/// completion runs last, so a caller that records „saved" from its own
/// completion can end up believing bytes are on disk that never got there.
/// Every caller must serialize its own writes per path; `AppView::
/// save_script`'s `script_save_in_flight` flag is the worked example.
///
/// A unique tmp name would NOT be a substitute for that discipline — this
/// is the corrected reason (T8 re-verify MINOR; the first version of this
/// paragraph argued from a stale copy of the `.gitignore` template in the
/// spec draft rather than from [`crate::workspace::GITIGNORE_TEMPLATE`],
/// which ships a blanket `*.tmp` and would happily cover a nonce). A nonce
/// fixes the first two failures — no shared tmp to truncate, no ENOENT
/// rename — but NOT the third and most dangerous: with two writes in
/// flight the `rename` that wins on disk still need not belong to the
/// continuation that runs last, so a caller recording „saved" from its own
/// completion can still end up believing bytes are on disk that never got
/// there. Only per-path serialization closes that, and once you have it a
/// nonce buys nothing. Keeping `<path>.tmp` also keeps the shipped
/// `.gitignore` — which `init_contents` never rewrites for an existing
/// workspace — matching without anyone re-editing their copy.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    use std::io::Write as _;
    let tmp = tmp_path_for(path);
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

    /// FINAL-REVIEW MINOR-3, the asymmetry `dbc-ui`'s
    /// `BLOCKED_WORKSPACE_SENTINEL` comment now states as a fact instead
    /// of over-claiming past it. `write_atomic` does NOT conjure its
    /// parent directory — a write into a folder that was never created
    /// FAILS, which is what makes a path aimed at a non-existent sentinel
    /// meaningful — while the pre-existing profile-store savers all
    /// `create_dir_all` first, which is what makes a first run work and is
    /// therefore why the sentinel is not, and cannot cheaply be made,
    /// unwritable.
    #[test]
    fn write_atomic_refuses_a_missing_parent_while_the_store_savers_create_one() {
        let td = tempfile::tempdir().unwrap();
        let missing = td.path().join("nikdy-nevytvoreno");
        assert!(write_atomic(&missing.join("a.sql"), b"x").is_err());
        assert!(!missing.exists(), "a failed write must not leave the folder behind");

        // The other half, in the crate that owns both: `AppConfig::save`
        // creates the parent, so the same path SUCCEEDS through it.
        let cfg = crate::AppConfig::default();
        let target = missing.join("config.toml");
        // The file does not exist, so a first save destroys nothing and the
        // witness mints (final-review MAJOR-2).
        let guard = crate::AppConfig::verify_savable(&target).unwrap();
        cfg.save(&target, &guard).unwrap();
        assert!(missing.join("config.toml").is_file());
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
    fn uppercase_fold_facts_this_doc_relies_on() {
        // `conflicting_entry_ci`'s doc justifies the uppercase fold by
        // classifying six pairs. That prose was WRONG in three of the six
        // for a while (the conclusion held; the reasoning did not), so the
        // classification is now executable.
        //
        // less  = lowercasing unifies the pair, uppercasing does not
        //         (the direction that could LOSE a collision — each of
        //         these was measured to be two distinct files on NTFS)
        // more  = uppercasing unifies the pair, lowercasing does not
        //         (an over-approximation: at worst a refused name)
        // apart = neither fold unifies
        // both  = both folds unify
        let verdict = |a: &str, b: &str| -> &'static str {
            match (a.to_lowercase() == b.to_lowercase(), a.to_uppercase() == b.to_uppercase()) {
                (true, false) => "less",
                (false, true) => "more",
                (false, false) => "apart",
                (true, true) => "both",
            }
        };
        for (a, b, want) in [
            ("ß", "ẞ", "less"),
            ("\u{212A}", "K", "less"),  // KELVIN SIGN
            ("\u{212B}", "å", "less"),  // ANGSTROM SIGN
            ("\u{00B5}", "\u{03BC}", "more"), // MICRO SIGN / GREEK SMALL MU
            ("\u{0130}", "i", "apart"), // LATIN CAPITAL I WITH DOT ABOVE
            ("\u{01C5}", "\u{01C6}", "both"), // ǅ / ǆ
            ("οδος", "οδοσ", "more"),   // the final-sigma false negative
        ] {
            assert_eq!(verdict(a, b), want, "pair {a:?}/{b:?}");
        }
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

    /// The single-writer contract, made checkable (workspace T8 review
    /// MAJOR-2): the scratch path is a pure function of the target, so two
    /// overlapping `write_atomic` calls for one file DO collide — every
    /// caller has to serialize its own writes per path. Pinned here so the
    /// hazard is a property of the rail, not a comment someone can drift
    /// away from; the `.tmp` ending is pinned because §W6.2's shipped
    /// `.gitignore` matches on it and init never rewrites an existing one.
    #[test]
    fn the_tmp_path_is_deterministic_which_is_why_callers_must_serialize() {
        let a = Path::new("D:").join("ws").join("config.toml");
        assert_eq!(tmp_path_for(&a), tmp_path_for(&a), "same target, same scratch file");
        assert_ne!(tmp_path_for(&a), tmp_path_for(&Path::new("D:").join("ws").join("views.toml")));
        assert!(tmp_path_for(&a).to_string_lossy().ends_with(".toml.tmp"));
        assert!(tmp_path_for(Path::new("vault.bin")).to_string_lossy().ends_with(".bin.tmp"));
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

/// Source-text audit: the `.tmp` naming rule has exactly one owner.
///
/// Belt, not braces — `tmp_path_for` being a function is the rail; this is
/// what stops a seventh store from quietly growing its own
/// `with_extension("toml.tmp")` again, which is how the rule ended up with
/// six copies in the first place. Nothing here can be checked by the type
/// system: the failure mode is a *new* line of code that never calls this
/// module at all.
///
/// SCOPE, stated so nobody over-reads it: it matches the literal text
/// `.tmp"` in non-`//` lines. A derivation spelled some other way (built
/// from a variable, or hidden in a block comment) walks straight past it.
/// It catches the shape that actually occurred, not every conceivable one.
#[cfg(test)]
mod tmp_naming_audit {
    use std::path::{Path, PathBuf};

    /// `(path suffix, occurrences, why it is allowed)`. Every entry is a
    /// literal FILENAME in a test, not a scratch-path derivation.
    const ALLOWED: &[(&str, usize, &str)] = &[(
        "dbc-ui/src/scripts.rs",
        1,
        "asserts a save left no scratch file behind — names the file, does not derive it",
    )];

    /// `CARGO_MANIFEST_DIR` is `<root>/crates/dbc-state`.
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("<root>/crates/dbc-state")
            .to_path_buf()
    }

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, &mut *out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    fn sources() -> Vec<PathBuf> {
        let mut out = Vec::new();
        rs_files(&workspace_root().join("crates"), &mut out);
        out.sort();
        out
    }

    /// The needle, assembled rather than written out, so this module's own
    /// source cannot match it and the audit stays honest about what it
    /// scans.
    fn needle() -> String {
        format!(".{}{}", "tmp", '"')
    }

    /// Lines that derive a scratch path, ignoring `//` comments.
    fn hits(src: &str) -> Vec<String> {
        let n = needle();
        src.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//") && l.contains(&n))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn tmp_naming_has_a_single_owner() {
        let files = sources();
        assert!(files.len() >= 30, "source walk found only {} files — it is not walking", files.len());

        let mut offenders: Vec<String> = Vec::new();
        for path in &files {
            let unix = path.to_string_lossy().replace('\\', "/");
            if unix.ends_with("dbc-state/src/fsutil.rs") {
                continue; // the rule's home
            }
            let src = std::fs::read_to_string(path).expect("readable source");
            let found = hits(&src).len();
            let budget = ALLOWED
                .iter()
                .find(|(suffix, _, _)| unix.ends_with(suffix))
                .map_or(0, |(_, n, _)| *n);
            if found > budget {
                offenders.push(format!("{unix}: {found} (allowed {budget})"));
            }
        }
        assert!(
            offenders.is_empty(),
            "these files derive a `.tmp` path themselves instead of calling \
             `fsutil::tmp_path_for`:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Non-vacuity, twice over: the needle must match a real derivation,
    /// and the one file the audit skips must actually contain the thing it
    /// is being skipped for — otherwise the exclusion above is silently
    /// doing nothing and the whole test could pass on an empty scan.
    #[test]
    fn the_audit_can_fail() {
        assert_eq!(hits("let tmp = p.with_extension(\"toml.tmp\");").len(), 1);
        assert_eq!(hits("s.push(\".tmp\");").len(), 1);
        assert!(hits("// a comment mentioning a \".tmp\" file").is_empty(), "comments are prose");

        let home = std::fs::read_to_string(workspace_root().join("crates/dbc-state/src/fsutil.rs"))
            .expect("fsutil.rs");
        assert!(!hits(&home).is_empty(), "the skipped file has nothing to skip — needle is wrong");
    }

    #[test]
    fn every_allowance_still_applies() {
        for (suffix, n, why) in ALLOWED {
            assert!(!why.trim().is_empty(), "{suffix} needs a reason");
            let path = workspace_root().join("crates").join(suffix);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("allowlisted {suffix} no longer exists — drop the entry"));
            assert_eq!(
                hits(&src).len(),
                *n,
                "{suffix} no longer has exactly {n} allowed occurrence(s); re-check, do not bump"
            );
        }
    }
}
