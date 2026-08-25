# Workspace Folder + Scripts Library Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers per task, a **sonnet** adversarial review per task, and a **default-model** final review once all tasks land (house staffing convention). NO docker, NO external server anywhere in this phase: every test is a pure `#[test]` over plain data or a `tempfile` directory.

**Goal:** One user-chosen, git-versioned **workspace folder** holds the entire working context — connections, app settings, view prefs / query params, the `.sql` scripts tree, and the encrypted Argon2id vault — with a Bruno-style scripts library on top of it; git stays 100% external to the app.

**Architecture:** Four layers. (1) **Shared fs rails** in `dbc-state` (`fsutil.rs`: one component validator, one Unicode-aware case-insensitive existence probe, one atomic writer) that *every* writer into a user-chosen folder reuses. (2) A **`dbc-state::workspace` module** — a pointer file in the profile dir, a marker file in the folder, folder classification, `resolve()` (Profile / Workspace / **Broken, never a silent fallback**), and a crash-safe init-by-copy whose marker is written LAST. (3) **`dbc-ui` wiring** — startup resolution + a blocking "workspace not found" modal, a Settings „Pracovní prostor" block with init/adopt confirm modals carrying the honest git warning, and a gated live in-place context swap. (4) The **scripts library** — a pinned „Skripty" sidebar section over `effective_scripts_root()` (workspace ⇒ `<workspace>/scripts`, profile ⇒ `AppConfig.scripts_dir`), editor binding with Ctrl+S, fs mutations, and a factored reuse of the unchanged G12 script-run confirm flow.

**Tech Stack:** Rust (edition 2021), GPUI pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`, `toml`/`serde` (already workspace deps), `tempfile` (dev-dep). **No new dependencies** — no `git2`, no `notify`, no `walkdir`, no `rfd`.

**Spec:** `docs/superpowers/specs/drafts/workspace-folder-design.md` — binding. Part W = workspace folder; Part S = the retained scripts design (its § numbers are referenced verbatim below). Line numbers in this plan were taken on branch `feature/scripts-library` at v0.21.0 (9589df6) — **always re-locate by symbol, never by line number**.

**Supersedes:** `docs/superpowers/plans/2026-08-25-scripts-library.md` (banner-marked). That file is history; this one drives the phase.

## Already implemented — do NOT redo

| Was | Where | State |
|---|---|---|
| T1 — `AppConfig.scripts_dir: Option<String>` (additive, `serde(default)` + `skip_serializing_if`, paired back-compat tests) | commit `a80c2e8` on branch `sc-t1-config`, file `crates/dbc-state/src/config.rs` | DONE — merge, don't rewrite |
| T2 — `crates/dbc-ui/src/scripts.rs` (scan with caps, symlink skip, `resolve_rel`, `validate_script_name`, create/rename/delete, atomic `write_script`, `read_script`, full tempfile suite) | commit `d129f6f` on branch `sc-t2-fsmod` | DONE — merge, don't rewrite |
| T2 review-fix round — Unicode-aware collision probe, **empty-rel root refusal**, case-SENSITIVE rename identity check, plus NITs | landing on `sc-t2-fsmod` while this plan is being written | Must be merged before Task 1; Task 1 MOVES its rails into `dbc-state`, it does not re-derive them |

**Task 0 merges all of the above into `feature/scripts-library` first.** If the review-fix round has NOT landed when Task 0 runs, stop and ask — Task 1 depends on its final shape.

## Global Constraints

- Worktree: `D:/workspace/home/db/.claude/worktrees/scripts`, branch `feature/scripts-library`. Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags (bare workspace builds only in the final task's gate).
- **Zero warnings** in plain AND test builds, debug AND release, for every crate touched. New `pub` items get doc comments. No `#[allow(dead_code)]` without a named removal owner in the same comment.
- **Git integration is permanently external** (binding user decision): no git dependency, no git subprocess, no git UI, no credentials, and — design §W6.4 — **not even read-only inspection of `.git/`**. If a step seems to want git status, stop: it is out of scope forever.
- **Security invariants** (design §W6.5): passwords exist ONLY inside `vault.bin` (Argon2id + ChaCha20Poly1305). No new file may carry a secret — not `config.toml`, not `dbc-workspace.toml`, not `workspace.toml`, not `.gitignore`, not a log line, not the status bar, not history. `config.rs::no_password_field_serialized` stays green; Task 2 adds the workspace-side equivalent.
- **Never destructive**: switching to a workspace COPIES profile files and never deletes, moves, or modifies them; switching back reuses them as-is. No task may add a "clean up the old profile" step.
- **Never a silent fallback**: a configured-but-unusable workspace must reach the user as a blocking modal with explicit choices (design §W4). No code path may quietly serve profile-mode connections when a pointer exists.
- **Shared rails, one implementation** (binding, from the T2 review's WIDER-SCOPE note): component validation, the Unicode-aware case-insensitive existence probe, and the atomic tmp+`sync_all`+rename write live in `crates/dbc-state/src/fsutil.rs` after Task 1. Every workspace file writer (init copies, marker, `.gitignore`, pointer) and `scripts.rs` call them. Writing a second `read_dir` loop or a second tmp+rename in this phase is a review-blocking defect.
- **Czech user-facing strings exactly as quoted** in the tasks below; errors use the `"error: …"` status prefix; notices reuse the `…`/`—` idiom. Warning/template texts are `const`s with byte-pinned tests.
- **Caps (design Part S §7):** `SCRIPTS_ENTRY_CAP = 2000`, `SCRIPTS_DEPTH_CAP = 12`, `SCRIPT_OPEN_CAP = 1 MiB` (editor open only), `SCRIPT_NAME_CAP = 80`.
- **Single-writer serialized files:** `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/schema_tree.rs`. Tasks touching the same file NEVER run in parallel worktrees — see the batch table.
- **Merge gate (every task):** `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` green, zero warnings. `dbc-mcp` is a REAL gate in this phase (it gets its own task), not a canary.
- **Versioning:** the final task bumps `[workspace.package] version` in the root `Cargo.toml` from `0.21.0` to **0.22.0** (re-verify free on `main` at merge time, house convention).

## Batches, parallelism, ordering

| Batch | Tasks | May run in parallel? | Touches |
|---|---|---|---|
| 0 | Task 0 (merge prep) | — (single) | git only |
| 1 | **Task 1 → Task 2** (workspace lane) ∥ **Task 3** (sidebar lane) | YES — separate worktrees; lanes share no file | lane A: `dbc-state/{fsutil,workspace,lib}.rs` + `dbc-ui/src/scripts.rs`; lane B: `dbc-ui/src/schema_tree.rs` |
| 2 | Task 4 (startup resolution + blocking modal) | — (single) | `main.rs`, `connections_ui.rs` |
| 3 | **Task 5** (Settings + init/adopt + swap) ∥ **Task 6** (dbc-mcp) | YES — separate crates | lane A: `main.rs`, `connections_ui.rs`; lane B: `dbc-mcp/src/main.rs` |
| 4 | Task 7 → Task 8 → Task 9 (scripts flip, editor binding, mutations + run) | NO — strictly sequential, all three own `main.rs` | `main.rs`, `connections_ui.rs`, + Task 7 also `schema_tree.rs` (safe: the batch is one sequential lane), Task 8 also `history_panel.rs`/`palette.rs` |
| 5 | Task 10 (sweep, 0.22.0, full gates) | — (single) | `Cargo.toml`, docs |

Ordering rationale (deviation from design §W9, recorded): the design listed the scripts tasks before the workspace flip and gave T4 a profile-only `effective_scripts_root` stub. This plan runs the workspace lane FIRST so `effective_scripts_root` lands complete (both arms) in Task 7 — one seam, written once, no stub to revisit.

---

### Task 0: Merge the landed T1/T2 work onto the phase branch

**Files:**
- No source edits — git only.

**Interfaces:**
- Consumes: branches `sc-t1-config` (commit `a80c2e8`) and `sc-t2-fsmod` (commit `d129f6f` + its review-fix round).
- Produces: `feature/scripts-library` containing `AppConfig.scripts_dir` and `crates/dbc-ui/src/scripts.rs` — every later task assumes both exist.

- [ ] **Step 1: Verify the review-fix round has landed on `sc-t2-fsmod`**

```bash
cd D:/workspace/home/db/.claude/worktrees/scripts
git log --oneline sc-t2-fsmod -5
git show sc-t2-fsmod:crates/dbc-ui/src/scripts.rs | grep -n "to_lowercase\|fn resolve_rel\|fn entry_exists\|parent_rel"
```

Expected: a commit after `d129f6f` whose `scripts.rs` shows (a) a `to_lowercase()`-based (not `eq_ignore_ascii_case`) collision probe, (b) `resolve_rel` refusing an empty rel, (c) a case-SENSITIVE identity check in `rename_entry`. **If any is missing: STOP and report — Task 1 moves this exact code and must not fork it.**

- [ ] **Step 2: Merge both branches**

```bash
cd D:/workspace/home/db/.claude/worktrees/scripts
git merge --no-ff sc-t1-config -m "merge: AppConfig.scripts_dir (scripts T1)"
git merge --no-ff sc-t2-fsmod -m "merge: scripts.rs fs module + review fixes (scripts T2)"
```

- [ ] **Step 3: Verify the gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings.

- [ ] **Step 4: Record the merged shape**

Read `crates/dbc-ui/src/scripts.rs` end to end and note the FINAL names of: the collision probe fn, the rel resolver(s) (including whatever the empty-rel refusal introduced for the "parent may be the root" case), and the atomic writer. Task 1 delegates to `dbc-state` using those exact names.

---

### Task 1: Shared fs rails in `dbc-state::fsutil` (one implementation, three rails)

**Files:**
- Create: `crates/dbc-state/src/fsutil.rs`
- Modify: `crates/dbc-state/src/lib.rs` (add `pub mod fsutil;`)
- Modify: `crates/dbc-ui/src/scripts.rs` (delegate the probe / component validation / atomic write; delete the duplicated bodies)

**Interfaces:**
- Consumes: `crate::config::StateError` (`StateError { message: String }`, `Display`, `From<std::io::Error>`).
- Produces (every later task uses these; no second implementation may exist):
  - `pub fn dbc_state::fsutil::join_component(base: &Path, component: &str) -> Result<PathBuf, StateError>`
  - `pub fn dbc_state::fsutil::entry_exists_ci(parent: &Path, name: &str) -> Result<bool, StateError>`
  - `pub fn dbc_state::fsutil::write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StateError>`

- [ ] **Step 1: Write the failing tests**

Create `crates/dbc-state/src/fsutil.rs` with ONLY the test module first (the file must not compile-pass yet):

```rust
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
        // ASCII-insensitive compare misses this pair; Unicode lowercasing does not.
        assert!(entry_exists_ci(td.path(), "čtení.sql").unwrap());
        assert!(entry_exists_ci(td.path(), "ČTENÍ.SQL").unwrap());
        assert!(!entry_exists_ci(td.path(), "jine.sql").unwrap());
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state fsutil`
Expected: FAIL — `cannot find function join_component` (module not declared / functions missing).

- [ ] **Step 3: Implement the rails**

Prepend to `crates/dbc-state/src/fsutil.rs`:

```rust
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

/// Case-INSENSITIVE existence probe inside ONE directory, Unicode-aware:
/// `str::to_lowercase` (full Unicode), NOT `eq_ignore_ascii_case` — the
/// latter misses `Č`/`č` and every other non-ASCII pair on a
/// case-insensitive volume (scripts T2 review). An unreadable directory
/// is an Err, never a silent `false`.
pub fn entry_exists_ci(parent: &Path, name: &str) -> Result<bool, StateError> {
    let rd = fs::read_dir(parent)
        .map_err(|e| err(format!("nelze číst složku {}: {e}", parent.display())))?;
    let want = name.to_lowercase();
    for ent in rd {
        let ent = ent
            .map_err(|e| err(format!("nelze číst složku {}: {e}", parent.display())))?;
        if ent.file_name().to_str().is_some_and(|n| n.to_lowercase() == want) {
            return Ok(true);
        }
    }
    Ok(false)
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
```

Add to `crates/dbc-state/src/lib.rs` (after the `mod config;` block, keeping the file's existing `mod` + `pub use` rhythm):

```rust
pub mod fsutil;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state fsutil`
Expected: PASS (4 tests).

- [ ] **Step 5: Delegate `scripts.rs` to the rails — delete the duplicates**

In `crates/dbc-ui/src/scripts.rs`, replace the bodies (names as recorded in Task 0 Step 4 — keep whatever spelling the review round left):

```rust
/// SECURITY (design Part S §7.1): joins a '/'-separated rel onto the root.
/// Each component goes through the SHARED rail
/// `dbc_state::fsutil::join_component`, so traversal, separators, drive
/// shapes and control characters are refused in exactly one place. An
/// EMPTY rel is refused here (T2 review): "the root itself" is a parent
/// position, not a target — use `resolve_parent_rel`.
pub fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("neplatná cesta".to_string());
    }
    let mut p = root.to_path_buf();
    for comp in rel.split('/') {
        p = dbc_state::fsutil::join_component(&p, comp).map_err(|e| e.message)?;
    }
    Ok(p)
}

/// Resolves a PARENT rel, where the empty string legitimately means the
/// root (creating at the top level). Never used for a target path.
pub fn resolve_parent_rel(root: &Path, parent_rel: &str) -> Result<PathBuf, String> {
    if parent_rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    resolve_rel(root, parent_rel)
}

fn entry_exists(parent: &Path, name: &str) -> Result<bool, String> {
    dbc_state::fsutil::entry_exists_ci(parent, name).map_err(|e| e.message)
}

/// Atomic write via the shared rail (see `dbc_state::fsutil::write_atomic`).
/// Last-writer-wins on external edits — by the user's own model git is the
/// history layer (design Part S §5.2).
pub fn write_script(path: &Path, text: &str) -> Result<(), String> {
    dbc_state::fsutil::write_atomic(path, text.as_bytes()).map_err(|e| e.message)
}
```

If the merged review round already introduced an equivalent of `resolve_parent_rel` under another name, KEEP its name and delegate only the body — do not add a second spelling. Delete the now-unused local `read_dir` probe loop and the local tmp+rename block; `use std::io::Write` may become unused (remove it if the compiler says so).

- [ ] **Step 6: Run the full scripts + state suites**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state -p dbc-ui`
Expected: PASS — every pre-existing `scripts.rs` test still green (error strings are byte-identical by construction: `"neplatná cesta"`, `"nelze číst složku {} : {e}"`, `"uložení selhalo: {e}"`). If a test fails on an error MESSAGE, fix the rail's message, not the test.

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-state/src/fsutil.rs crates/dbc-state/src/lib.rs crates/dbc-ui/src/scripts.rs
git commit -m "feat: shared fs rails in dbc-state::fsutil, scripts.rs delegates (workspace T1)"
```

---

### Task 2: `dbc-state::workspace` — pointer, marker, classification, resolution, crash-safe init

**Files:**
- Create: `crates/dbc-state/src/workspace.rs`
- Modify: `crates/dbc-state/src/lib.rs` (add `pub mod workspace;`)

**Interfaces:**
- Consumes: `dbc_state::fsutil::{join_component, entry_exists_ci, write_atomic}` (Task 1); `crate::config::{StateError, default_config_path}`; `crate::vault::default_vault_path`; `crate::view_prefs::default_view_prefs_path`; `crate::params::default_param_values_path`; `crate::history::default_history_path`.
- Produces (Tasks 4, 5, 6 consume these EXACT names):
  - `pub struct Paths { pub config: PathBuf, pub vault: PathBuf, pub views: PathBuf, pub params: PathBuf, pub history: PathBuf }` (`Debug + Clone + PartialEq + Eq`)
  - `pub enum Resolution { Profile(Paths), Workspace { root: PathBuf, paths: Paths }, Broken { root: Option<PathBuf>, reason: String } }`
  - `pub enum Classification { Workspace, Empty, NonEmpty, FutureFormat(u32), Unreadable(String) }`
  - `pub fn profile_dir() -> PathBuf`, `pub fn pointer_path() -> PathBuf`
  - `pub fn profile_paths() -> Paths`, `pub fn workspace_paths(root: &Path) -> Paths`
  - `pub fn read_pointer(pointer: &Path) -> Result<Option<PathBuf>, StateError>`
  - `pub fn write_pointer(pointer: &Path, root: &Path) -> Result<(), StateError>`
  - `pub fn clear_pointer(pointer: &Path) -> Result<(), StateError>`
  - `pub fn classify(root: &Path) -> Classification`
  - `pub fn resolve_at(pointer: &Path) -> Resolution`, `pub fn resolve() -> Resolution`
  - `pub fn init_contents(root: &Path, from: &Paths) -> Result<(), StateError>`
  - `pub fn write_marker(root: &Path) -> Result<(), StateError>`
  - `pub fn init_workspace(root: &Path, from: &Paths) -> Result<(), StateError>`
  - `pub const MARKER_FILE: &str`, `POINTER_FILE: &str`, `SCRIPTS_SUBDIR: &str`, `GITIGNORE_FILE: &str`, `WORKSPACE_FORMAT: u32`, `GITIGNORE_TEMPLATE: &str`

- [ ] **Step 1: Write the failing tests**

Create `crates/dbc-state/src/workspace.rs` containing ONLY this test module for now:

```rust
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

        assert_eq!(std::fs::read_to_string(root.join("config.toml")).unwrap(), "theme = \"dark\"\n");
        assert_eq!(std::fs::read(root.join("vault.bin")).unwrap(), std::fs::read(&from.vault).unwrap());
        assert!(root.join("views.toml").exists());
        assert!(root.join("params.toml").exists());
        assert!(root.join(SCRIPTS_SUBDIR).is_dir());
        assert!(root.join(GITIGNORE_FILE).exists());
        assert_eq!(classify(&root), Classification::Workspace);

        // NEVER DESTRUCTIVE: every profile file still there, byte-identical.
        assert!(from.config.exists() && from.vault.exists() && from.views.exists() && from.params.exists());
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state workspace`
Expected: FAIL — `cannot find type Paths` / `cannot find function resolve_at` (module body missing).

- [ ] **Step 3: Implement the module**

Prepend to `crates/dbc-state/src/workspace.rs`:

```rust
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
    pub config: PathBuf,
    pub vault: PathBuf,
    pub views: PathBuf,
    pub params: PathBuf,
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
    Workspace { root: PathBuf, paths: Paths },
    /// A pointer exists but its target is unusable. The caller MUST
    /// surface this and let the user choose (§W4) — falling back to the
    /// profile silently would put a muscle-memory click one step from the
    /// wrong „prod".
    Broken { root: Option<PathBuf>, reason: String },
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
    let p: PointerFile =
        toml::from_str(&raw).map_err(|e| err(format!("ukazatel na pracovní prostor je poškozený: {e}")))?;
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
        Classification::Empty | Classification::NonEmpty => Resolution::Broken {
            root: Some(root),
            reason: format!("chybí {MARKER_FILE}"),
        },
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
        std::fs::create_dir(&scripts)
            .map_err(|e| err(format!("vytvoření složky selhalo: {e}")))?;
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
    let body = toml::to_string(&Marker { format: WORKSPACE_FORMAT })
        .map_err(|e| err(e.to_string()))?;
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
```

Add to `crates/dbc-state/src/lib.rs`:

```rust
pub mod workspace;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state workspace`
Expected: PASS (18 tests). If `marker_holds_only_the_format_key` fails on a trailing newline, adjust the assertion to the exact `toml::to_string` output — do NOT hand-roll the TOML.

- [ ] **Step 5: Full state gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state`
Expected: PASS, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-state/src/workspace.rs crates/dbc-state/src/lib.rs
git commit -m "feat: dbc-state::workspace — pointer, marker, classify, resolve, crash-safe init (workspace T2)"
```

---

### Task 3: Sidebar lane — `schema_tree.rs` additive types/state for the „Skripty" section (dark)

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs` (types, pure emission, expand plumbing, tests)

Runs in PARALLEL with Tasks 1→2 (separate worktree, separate file). It lands **dark and inert**: `flatten_sidebar`'s new `scripts` argument is `None` at the only production call site (`SchemaTree::render`), so not one row changes on screen and every pre-existing sidebar test keeps its exact output. **This task does NOT touch `main.rs`** — no `TreeEvent` variant is added here (Part S §4: "handlers land with the flip — the exhaustive match forces same-task"), because `AppView::on_tree_event`'s match is exhaustive and `main.rs` belongs to Task 4's batch.

**Interfaces:**
- Consumes: `crate::scripts::{ScriptEntry, ScriptScan}` (Task 0's merged T2 module — `ScriptEntry { rel: String, is_dir: bool, depth: usize }`, `ScriptScan { entries: Vec<ScriptEntry>, truncated: bool, depth_clipped: bool }`); the existing private helper `fn name_matches(name: &str, filter_lc: &str) -> bool`.
- Produces (Task 7 consumes these EXACT names):
  - `pub enum ScriptsListState { NotLoaded, Loading { generation: u64 }, Error(String), Loaded { entries: Vec<ScriptEntry>, truncated: bool, depth_clipped: bool } }`
  - `SidebarRow::ScriptsRoot`, `SidebarRow::ScriptFolder { rel: String }`, `SidebarRow::ScriptFile { rel: String }`, `SidebarRow::ScriptNotice { text: String, open_settings: bool }`
  - `OuterId::Scripts`, `OuterId::ScriptFolder(String)`
  - `pub fn emit_scripts_section(out: &mut Vec<SidebarFlatRow>, state: &ScriptsListState, configured: bool, outer_expanded: &HashSet<OuterId>, filter: &str)`
  - `pub fn flatten_sidebar(…, admin: AdminEntry, scripts: Option<(&ScriptsListState, bool)>) -> Vec<SidebarFlatRow>` — the `scripts` parameter is appended LAST
  - `impl SchemaTree`: `pub fn begin_scripts_scan(&mut self, cx: &mut Context<Self>) -> u64`, `pub fn finish_scripts_scan(&mut self, generation: u64, result: Result<crate::scripts::ScriptScan, String>, cx: &mut Context<Self>)`, `pub fn set_scripts_configured(&mut self, configured: bool, cx: &mut Context<Self>)`, `pub fn reset_scripts(&mut self, cx: &mut Context<Self>)`, `pub fn scripts_needs_scan(&self) -> bool`

- [ ] **Step 1: Write the failing tests**

Append to the existing `#[cfg(test)] mod tests` in `crates/dbc-ui/src/schema_tree.rs` (it already has `use super::*;` plus the `conn`/`grouped`/`loaded_states` helpers):

```rust
    // ---------- Scripts section (workspace T3, dark) ----------

    fn script_entries(specs: &[(&str, bool, usize)]) -> Vec<crate::scripts::ScriptEntry> {
        specs
            .iter()
            .map(|(rel, is_dir, depth)| crate::scripts::ScriptEntry {
                rel: (*rel).to_string(),
                is_dir: *is_dir,
                depth: *depth,
            })
            .collect()
    }

    fn loaded_scripts(specs: &[(&str, bool, usize)]) -> ScriptsListState {
        ScriptsListState::Loaded {
            entries: script_entries(specs),
            truncated: false,
            depth_clipped: false,
        }
    }

    fn emit(
        state: &ScriptsListState,
        configured: bool,
        expanded: &[OuterId],
        filter: &str,
    ) -> Vec<SidebarFlatRow> {
        let set: HashSet<OuterId> = expanded.iter().cloned().collect();
        let mut out = Vec::new();
        emit_scripts_section(&mut out, state, configured, &set, filter);
        out
    }

    #[test]
    fn scripts_root_is_collapsed_by_default_and_shows_only_its_header() {
        let rows = emit(&loaded_scripts(&[("a.sql", false, 0)]), true, &[], "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, SidebarRow::ScriptsRoot);
        assert_eq!(rows[0].2, "Skripty");
        assert!(rows[0].3, "the root row is expandable");
    }

    #[test]
    fn expanded_unconfigured_root_shows_the_settings_pointer_notice() {
        let rows = emit(&ScriptsListState::NotLoaded, false, &[OuterId::Scripts], "");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1].0,
            SidebarRow::ScriptNotice {
                text: "složka skriptů není nastavena — klikněte pro Nastavení".to_string(),
                open_settings: true,
            }
        );
    }

    #[test]
    fn loading_and_error_states_render_their_own_notice_rows() {
        let rows = emit(&ScriptsListState::Loading { generation: 3 }, true, &[OuterId::Scripts], "");
        assert_eq!(rows[1].2, "Načítám skripty…");
        let rows =
            emit(&ScriptsListState::Error("složka zmizela".into()), true, &[OuterId::Scripts], "");
        // The `error:` prefix is the Notice color sentinel — keep it literal.
        assert_eq!(rows[1].2, "error: složka zmizela");
        assert_eq!(
            rows[1].0,
            SidebarRow::ScriptNotice {
                text: "error: složka zmizela".to_string(),
                open_settings: false,
            }
        );
    }

    #[test]
    fn an_empty_loaded_library_says_so() {
        let rows = emit(&loaded_scripts(&[]), true, &[OuterId::Scripts], "");
        assert_eq!(rows[1].2, "žádné skripty (*.sql)");
    }

    #[test]
    fn children_appear_only_under_expanded_folders_at_depth_one_plus_entry_depth() {
        let state = loaded_scripts(&[
            ("prod", true, 0),
            ("prod/reporting.sql", false, 1),
            ("dotaz.sql", false, 0),
        ]);
        // Folder collapsed: its child is hidden, the sibling file is not.
        let rows = emit(&state, true, &[OuterId::Scripts], "");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(labels, vec!["Skripty", "prod", "dotaz.sql"]);
        assert_eq!(rows[1].1, 1, "a root-level entry sits at 1 + depth 0");

        // Folder expanded: the child splices in at 1 + depth 1.
        let rows = emit(
            &state,
            true,
            &[OuterId::Scripts, OuterId::ScriptFolder("prod".to_string())],
            "",
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(labels, vec!["Skripty", "prod", "reporting.sql", "dotaz.sql"]);
        assert_eq!(rows[2].1, 2);
        assert_eq!(rows[2].0, SidebarRow::ScriptFile { rel: "prod/reporting.sql".to_string() });
    }

    #[test]
    fn a_grandchild_stays_hidden_when_any_ancestor_is_collapsed() {
        let state = loaded_scripts(&[("a", true, 0), ("a/b", true, 1), ("a/b/c.sql", false, 2)]);
        // Only the INNER folder is expanded — the outer one is not, so
        // nothing below `a` may appear.
        let rows = emit(
            &state,
            true,
            &[OuterId::Scripts, OuterId::ScriptFolder("a/b".to_string())],
            "",
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(labels, vec!["Skripty", "a"]);
    }

    #[test]
    fn filter_auto_expands_folders_and_drops_childless_non_matching_ones() {
        let state = loaded_scripts(&[
            ("prod", true, 0),
            ("prod/trzby.sql", false, 1),
            ("test", true, 0),
            ("test/smoke.sql", false, 1),
        ]);
        let rows = emit(&state, true, &[OuterId::Scripts], "trzby");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        // `prod` survives because a descendant matched (auto-expand); `test`
        // is childless after filtering and its own name misses.
        assert_eq!(labels, vec!["Skripty", "prod", "trzby.sql"]);
    }

    #[test]
    fn filter_keeps_a_folder_whose_own_name_matches_even_with_no_children() {
        let state = loaded_scripts(&[("prod", true, 0), ("prod/trzby.sql", false, 1)]);
        let rows = emit(&state, true, &[OuterId::Scripts], "prod");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(labels, vec!["Skripty", "prod"]);
    }

    #[test]
    fn cap_notices_render_after_the_entries() {
        let state = ScriptsListState::Loaded {
            entries: script_entries(&[("a.sql", false, 0)]),
            truncated: true,
            depth_clipped: true,
        };
        let rows = emit(&state, true, &[OuterId::Scripts], "");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Skripty",
                "a.sql",
                "… zobrazeno prvních 2000 položek — zmenšete knihovnu skriptů",
                "… některé podsložky jsou příliš hluboko (limit 12 úrovní)",
            ]
        );
    }

    #[test]
    fn flatten_sidebar_with_none_scripts_is_byte_identical_to_before() {
        // The DARK contract: Task 3 changes nothing on screen.
        let conns = vec![conn("c1", "Prod", &[])];
        let with_none = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden, None);
        let with_some = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden,
            Some((&ScriptsListState::NotLoaded, false)));
        assert!(!with_none.iter().any(|r| matches!(r.0, SidebarRow::ScriptsRoot)));
        assert_eq!(with_some[0].0, SidebarRow::ScriptsRoot, "Some(..) emits the section");
        assert_eq!(&with_some[1..], &with_none[..], "everything else is unchanged");
    }

    #[test]
    fn scripts_section_is_emitted_after_favourites_and_before_the_connection_roots() {
        let conns = vec![conn("c1", "Prod", &[])];
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden,
            Some((&ScriptsListState::NotLoaded, true)));
        let scripts_ix = rows.iter().position(|r| matches!(r.0, SidebarRow::ScriptsRoot)).unwrap();
        let conn_ix = rows
            .iter()
            .position(|r| matches!(&r.0, SidebarRow::Connection { conn_id } if conn_id == "c1"))
            .unwrap();
        assert!(scripts_ix < conn_ix);
    }
```

`conn(..)`/`grouped(..)` are the existing helpers in that test module — reuse them verbatim; if `conn` has a different arity in the merged file, adapt the CALL, never add a second helper. `SidebarFlatRow` is `(SidebarRow, usize, String, bool)`, so `r.0`/`r.1`/`r.2`/`r.3` are row/depth/label/expandable.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui scripts_root`
Expected: FAIL to compile — `cannot find type ScriptsListState`, `no variant named ScriptsRoot`, `this function takes 8 arguments but 9 arguments were supplied`.

- [ ] **Step 3: Add the types**

In `crates/dbc-ui/src/schema_tree.rs`, extend `SidebarRow` (right after the existing `Notice` arm):

```rust
    /// Scripts library (Part S §3.3): the pinned „Skripty" section header.
    /// Unparameterized, like `Pinned(NodeId::AdminRoot)` — the section is
    /// GLOBAL (it does not depend on the active scope: scripts are files,
    /// not database objects).
    ScriptsRoot,
    /// A folder inside the scripts library. `rel` is '/'-separated on every
    /// platform (the `ScriptEntry::rel` convention) and doubles as the
    /// expand key (`OuterId::ScriptFolder`).
    ScriptFolder { rel: String },
    /// A `*.sql` file inside the scripts library.
    ScriptFile { rel: String },
    /// Scripts-section notice (unconfigured / loading / error / cap
    /// disclosure). `open_settings` marks the ONE clickable kind — the
    /// unconfigured pointer row, which opens „Nastavení" (Part S §1.4:
    /// discoverability without a wizard).
    ScriptNotice { text: String, open_settings: bool },
```

Extend `OuterId` (same file, after `Favourites`):

```rust
    /// The „Skripty" section itself — LAZY polarity (presence = expanded),
    /// like `Connection`/`Database`/`Favourites`, NOT the inverted
    /// `Folder` polarity: the section is collapsed by default (Part S §1.4).
    Scripts,
    /// One scripts-library folder, keyed by its '/'-separated `rel`. Lazy
    /// polarity too — the scripts tree is browsed, not pre-opened.
    ScriptFolder(String),
```

Add the state machine next to `DbListState`:

```rust
/// Lazy-scan state machine for the scripts library (Part S §3.3) — the same
/// family as `DbListState`, `generation` guarding against a stale in-flight
/// scan clobbering a newer dispatch. There is exactly ONE of these on
/// `SchemaTree`: the library is global, not per-connection (Part S §1.1).
pub enum ScriptsListState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { entries: Vec<crate::scripts::ScriptEntry>, truncated: bool, depth_clipped: bool },
}
```

- [ ] **Step 4: Write the pure emission**

Add above `flatten_sidebar`:

```rust
/// The last '/'-component of a rel — what a scripts row displays. Rels
/// always originate from the scan (single `file_name()` components joined
/// with '/'), so this never has to canonicalize anything.
fn script_row_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Visibility rule for one scanned entry: EVERY ancestor folder rel must be
/// expanded. Under an active filter every folder counts as expanded — the
/// sidebar-wide auto-expand contract (Part S §3.3), so a match buried three
/// levels down is actually reachable.
fn script_ancestors_expanded(
    rel: &str,
    outer_expanded: &HashSet<OuterId>,
    filter_active: bool,
) -> bool {
    if filter_active {
        return true;
    }
    let mut parts: Vec<&str> = rel.split('/').collect();
    parts.pop(); // the entry's own name is not one of its ancestors
    let mut prefix = String::new();
    for p in parts {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(p);
        if !outer_expanded.contains(&OuterId::ScriptFolder(prefix.clone())) {
            return false;
        }
    }
    true
}

/// Filter pass: drop a `ScriptFolder` row left with NO children whose own
/// name also misses. Walked BACKWARD from the end so a folder emptied by
/// this very pass can itself be dropped (nested misses collapse fully) —
/// the `flatten_sidebar` childless-row idiom, adapted to a depth-first
/// splice where children always follow their parent at a greater depth.
fn prune_childless_script_folders(rows: &mut Vec<SidebarFlatRow>, first: usize, filter_lc: &str) {
    let mut i = rows.len();
    while i > first {
        i -= 1;
        let (row, depth, label, _) = &rows[i];
        if !matches!(row, SidebarRow::ScriptFolder { .. }) {
            continue;
        }
        let has_child = rows.get(i + 1).is_some_and(|(_, d, _, _)| *d > *depth);
        if !has_child && !name_matches(label, filter_lc) {
            rows.remove(i);
        }
    }
}

/// Emits the pinned „Skripty" section (Part S §3.3/§3.4). Pure, GPUI-free,
/// never fetches — typing in the speed search can only ever narrow what the
/// last scan produced.
pub fn emit_scripts_section(
    out: &mut Vec<SidebarFlatRow>,
    state: &ScriptsListState,
    configured: bool,
    outer_expanded: &HashSet<OuterId>,
    filter: &str,
) {
    out.push((SidebarRow::ScriptsRoot, 0, "Skripty".to_string(), true));
    if !outer_expanded.contains(&OuterId::Scripts) {
        return;
    }
    fn notice(out: &mut Vec<SidebarFlatRow>, text: String, open_settings: bool) {
        out.push((SidebarRow::ScriptNotice { text: text.clone(), open_settings }, 1, text, false));
    }
    if !configured {
        notice(out, "složka skriptů není nastavena — klikněte pro Nastavení".to_string(), true);
        return;
    }
    match state {
        // The expand handler is dispatching the scan; nothing to show yet.
        ScriptsListState::NotLoaded => {}
        ScriptsListState::Loading { .. } => notice(out, "Načítám skripty…".to_string(), false),
        // The `error:` prefix is the Notice COLOR SENTINEL (the row render
        // dispatches on it literally) — never reword it away.
        ScriptsListState::Error(e) => notice(out, format!("error: {e}"), false),
        ScriptsListState::Loaded { entries, truncated, depth_clipped } => {
            if entries.is_empty() {
                notice(out, "žádné skripty (*.sql)".to_string(), false);
            }
            let filter_lc = filter.to_lowercase();
            let filter_active = !filter_lc.is_empty();
            let first = out.len();
            for e in entries {
                if !script_ancestors_expanded(&e.rel, outer_expanded, filter_active) {
                    continue;
                }
                let name = script_row_name(&e.rel).to_string();
                if filter_active && !e.is_dir && !name_matches(&name, &filter_lc) {
                    continue;
                }
                let row = if e.is_dir {
                    SidebarRow::ScriptFolder { rel: e.rel.clone() }
                } else {
                    SidebarRow::ScriptFile { rel: e.rel.clone() }
                };
                out.push((row, 1 + e.depth, name, e.is_dir));
            }
            if filter_active {
                prune_childless_script_folders(out, first, &filter_lc);
            }
            if *truncated {
                notice(
                    out,
                    "… zobrazeno prvních 2000 položek — zmenšete knihovnu skriptů".to_string(),
                    false,
                );
            }
            if *depth_clipped {
                notice(
                    out,
                    "… některé podsložky jsou příliš hluboko (limit 12 úrovní)".to_string(),
                    false,
                );
            }
        }
    }
}
```

Then widen `flatten_sidebar`: append the parameter

```rust
    admin: AdminEntry,
    /// Scripts library section (Part S §1.4/§3.3): `None` keeps the section
    /// out of the sidebar entirely — the state Task 3 ships in, and the
    /// state every pre-existing test asserts against. `Some((state,
    /// configured))` = live (Task 7's flip); `configured` is
    /// `AppView::effective_scripts_root().is_some()`.
    scripts: Option<(&ScriptsListState, bool)>,
```

and splice the emission immediately AFTER the Favourites block, BEFORE the CLI synthetic root:

```rust
    // Scripts library (Part S §1.4): a third pinned root section, after
    // „Oblíbené" and before the CLI/connection roots. GLOBAL — unlike the
    // pinned rows above it, it does not depend on `active`.
    if let Some((state, configured)) = scripts {
        emit_scripts_section(&mut out, state, configured, outer_expanded, filter);
    }
```

- [ ] **Step 5: Fix the exhaustive matches and the call sites**

Compiler-guided sweep inside `schema_tree.rs` — each of these matches is exhaustive today and MUST gain an arm:

```rust
// toggle_outer — the new rows carry real expand keys.
            SidebarRow::ScriptsRoot => OuterId::Scripts,
            SidebarRow::ScriptFolder { rel } => OuterId::ScriptFolder(rel.clone()),
```

```rust
// handle_chevron — Task 3 only TOGGLES. The scan dispatch (emitting
// `TreeEvent::ScriptsRefresh` when the slot is `NotLoaded`/`Error`) lands
// in Task 7 together with `main.rs`'s handler, because `TreeEvent`'s match
// in `AppView::on_tree_event` is exhaustive and `main.rs` is not this
// task's file.
            SidebarRow::ScriptsRoot | SidebarRow::ScriptFolder { .. } => self.toggle_outer(row),
            SidebarRow::ScriptFile { .. } | SidebarRow::ScriptNotice { .. } => {}
```

```rust
// row_is_expanded
            SidebarRow::ScriptsRoot => self.outer_expanded.contains(&OuterId::Scripts),
            SidebarRow::ScriptFolder { rel } => {
                // Filter auto-expands, mirroring `emit_scripts_section`.
                !self.filter.is_empty()
                    || self.outer_expanded.contains(&OuterId::ScriptFolder(rel.clone()))
            }
            SidebarRow::ScriptFile { .. } | SidebarRow::ScriptNotice { .. } => false,
```

```rust
// handle_double_click: ScriptsRoot/ScriptFolder fall into the existing
// "otherwise toggle expand" tail — no new arm needed beyond what the
// compiler asks for. ScriptFile's "open into the editor" is Task 8's.
// handle_single_click: plain selection for ScriptsRoot/ScriptFolder/
// ScriptFile (the existing `_ =>` select tail covers them); ScriptNotice is
// inert here — its retry / open-„Nastavení" click is Task 7's, with the
// event:
            SidebarRow::ScriptNotice { .. } => {}
// row_in_active_scope / favourite_object_for — scripts rows are NEVER in
// scope and never favouritable (they are files, not database objects):
            SidebarRow::ScriptsRoot
            | SidebarRow::ScriptFolder { .. }
            | SidebarRow::ScriptFile { .. }
            | SidebarRow::ScriptNotice { .. } => false,
```

Add the state to the `SchemaTree` struct. Each field carries its removal owner — a `#[allow(dead_code)]` without a named owner in the same comment is a review-blocking defect per the Global Constraints:

```rust
    /// Scripts library scan state (Part S §3.3). DARK UNTIL TASK 7 —
    /// removal owner: Task 7 (the scripts flip) deletes this attribute when
    /// `render` starts passing `Some(..)` to `flatten_sidebar`.
    #[allow(dead_code)]
    scripts: ScriptsListState,
    /// Whether a scripts root exists at all — profile mode:
    /// `AppConfig.scripts_dir.is_some()`; workspace mode: always `true`
    /// (`<workspace>/scripts` is created by init). DARK UNTIL TASK 7 —
    /// removal owner: Task 7.
    #[allow(dead_code)]
    scripts_configured: bool,
    /// Stale-scan guard, the `DbListState::Loading { generation }` shape.
    /// DARK UNTIL TASK 7 — removal owner: Task 7.
    #[allow(dead_code)]
    scripts_generation: u64,
```

and to `SchemaTree::new`: `scripts: ScriptsListState::NotLoaded, scripts_configured: false, scripts_generation: 0,`.

Then a dedicated impl block (ONE attribute, ONE owner note):

```rust
/// Scripts-library state API. DARK UNTIL TASK 7 — removal owner: Task 7
/// (the scripts flip) deletes this `#[allow(dead_code)]` when `main.rs`
/// starts calling these from `start_scripts_scan` / `apply_context`.
#[allow(dead_code)]
impl SchemaTree {
    /// Moves the slot to `Loading` and returns the generation the caller
    /// must hand back to `finish_scripts_scan` (older results are dropped).
    pub fn begin_scripts_scan(&mut self, cx: &mut Context<Self>) -> u64 {
        self.scripts_generation = self.scripts_generation.wrapping_add(1);
        self.scripts = ScriptsListState::Loading { generation: self.scripts_generation };
        cx.notify();
        self.scripts_generation
    }

    /// Applies a scan result, DROPPING it when a newer scan has since been
    /// dispatched (`DbListState`'s generation contract verbatim).
    pub fn finish_scripts_scan(
        &mut self,
        generation: u64,
        result: Result<crate::scripts::ScriptScan, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.scripts_generation {
            return;
        }
        self.scripts = match result {
            Ok(scan) => ScriptsListState::Loaded {
                entries: scan.entries,
                truncated: scan.truncated,
                depth_clipped: scan.depth_clipped,
            },
            Err(e) => ScriptsListState::Error(e),
        };
        cx.notify();
    }

    /// Pushes „is there a scripts root at all" (Task 7 calls this from
    /// `refresh_tree_context`). Changing it never scans by itself.
    pub fn set_scripts_configured(&mut self, configured: bool, cx: &mut Context<Self>) {
        if self.scripts_configured != configured {
            self.scripts_configured = configured;
            cx.notify();
        }
    }

    /// Back to `NotLoaded`, bumping the generation so an in-flight scan of
    /// the OLD root can never land, and dropping the per-folder expand keys
    /// (they name paths that no longer exist). The context swap (§W3.4) and
    /// „Odebrat" both need exactly this.
    pub fn reset_scripts(&mut self, cx: &mut Context<Self>) {
        self.scripts_generation = self.scripts_generation.wrapping_add(1);
        self.scripts = ScriptsListState::NotLoaded;
        self.outer_expanded.retain(|id| !matches!(id, OuterId::ScriptFolder(_)));
        cx.notify();
    }

    /// True when expanding (or retrying) the root should dispatch a scan.
    pub fn scripts_needs_scan(&self) -> bool {
        matches!(self.scripts, ScriptsListState::NotLoaded | ScriptsListState::Error(_))
    }

}
```

Finally the call sites: `SchemaTree::render`'s `flatten_sidebar(…)` gains a trailing `None` (**this is what keeps the task dark** — do NOT pass `Some(..)` here, that single line is Task 7's flip), and every pre-existing `flatten_sidebar` call in the test module gains a trailing `, None`. Purely mechanical, compiler-listed.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: PASS — the 11 new tests plus every pre-existing sidebar test unchanged (the dark contract).
Then `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` and `… build -p dbc-ui --release`: zero warnings in the plain builds too (the three `#[allow(dead_code)]` markers are exactly what makes that true — and exactly what Task 7 must delete).

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/src/schema_tree.rs
git commit -m "feat: schema_tree scripts section — types, state, pure emission (dark) (workspace T3)"
```

---

### Task 4: Startup resolution + the blocking „Pracovní prostor nenalezen" modal

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` (`fn main()`, `struct AppView`, the new pure `startup_context`, `AppView::apply_context`, `AppView::clear_active_connection`, `AppView::open_workspace_missing_modal`, `AppView::pick_workspace_for_recovery`, `AppView::use_local_profile`)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::WorkspaceMissing`, `modal_confirm_kind`, `on_cancel_query`'s Esc allow-list, `render_workspace_missing_panel`)

Batch 2, ALONE — it owns both single-writer UI files.

**THE RAIL THIS TASK EXISTS FOR (design §W4, Global Constraints):** a configured-but-unusable workspace must NEVER resolve to profile-mode data. Not "load the profile and show a toast" — the app starts with an EMPTY default config, opens NO config/views/params store, and shows a modal that is not Esc-closable, not Enter-confirmable, and whose only exits are the three explicit choices. If a step here starts to look like `unwrap_or_else(|| profile_paths())`, it is the defect this whole task is written to prevent.

**What this modal is NOT for (design §W4, last paragraph):** corrupt-but-PRESENT workspace files. A workspace whose marker is valid but whose `config.toml` fails to parse resolves to `Resolution::Workspace` and follows the existing per-store degrade postures — config: status-bar error + save refusal; views/params: feature off. Those are content errors inside the RIGHT context, not a wrong-context risk, and `classify` never returns them as `Broken` by construction (Task 2).

**Interfaces:**
- Consumes: `dbc_state::workspace::{Resolution, Paths, Classification, classify, resolve, profile_paths, workspace_paths, profile_dir, pointer_path, read_pointer, write_pointer, clear_pointer}` (Task 2, exact names).
- Produces:
  - `pub(crate) struct StartupContext { pub paths: dbc_state::workspace::Paths, pub workspace_root: Option<PathBuf>, pub blocked: Option<(Option<PathBuf>, String)> }`
  - `pub(crate) fn startup_context(res: dbc_state::workspace::Resolution) -> StartupContext` (pure)
  - `pub(crate) fn blocked_paths(root: Option<&Path>) -> dbc_state::workspace::Paths` (pure)
  - `AppView.workspace_root: Option<PathBuf>` (field)
  - `pub(crate) fn AppView::apply_context(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>)` — **THE §W3.4 live in-place swap, the one seam. Task 5 calls this exact signature; Task 7 adds one line to its body.**
  - `fn AppView::clear_active_connection(&mut self, cx: &mut Context<Self>)`
  - `connections_ui::ModalState::WorkspaceMissing { root: Option<PathBuf>, reason: String, error: Option<String> }`

- [ ] **Step 1: Write the failing tests**

Add a new test module at the end of `crates/dbc-ui/src/main.rs` (pure — no GPUI, no fs, no tempdir; `Resolution` is plain data):

```rust
#[cfg(test)]
mod workspace_startup_tests {
    use super::*;
    use dbc_state::workspace::{profile_paths, workspace_paths, Resolution};

    #[test]
    fn no_pointer_starts_in_profile_mode_with_todays_paths() {
        let ctx = startup_context(Resolution::Profile(profile_paths()));
        assert_eq!(ctx.paths, profile_paths());
        assert_eq!(ctx.workspace_root, None);
        assert!(ctx.blocked.is_none());
    }

    #[test]
    fn a_valid_pointer_starts_in_workspace_mode_over_the_folder() {
        let root = PathBuf::from("D:\\ws");
        let ctx = startup_context(Resolution::Workspace {
            root: root.clone(),
            paths: workspace_paths(&root),
        });
        assert_eq!(ctx.paths.config, root.join("config.toml"));
        assert_eq!(ctx.paths.vault, root.join("vault.bin"));
        // §W5: history stays machine-local even in workspace mode.
        assert_eq!(ctx.paths.history, profile_paths().history);
        assert_eq!(ctx.workspace_root, Some(root));
        assert!(ctx.blocked.is_none());
    }

    #[test]
    fn a_broken_pointer_blocks_and_never_yields_a_single_profile_path() {
        // THE never-silent-fallback rail (design §W4).
        let root = PathBuf::from("D:\\ws-gone");
        let ctx = startup_context(Resolution::Broken {
            root: Some(root.clone()),
            reason: "složka neexistuje".to_string(),
        });
        assert_eq!(ctx.blocked, Some((Some(root.clone()), "složka neexistuje".to_string())));
        assert_eq!(ctx.workspace_root, None, "a broken workspace is NOT an active workspace");
        let p = profile_paths();
        for got in [&ctx.paths.config, &ctx.paths.vault, &ctx.paths.views, &ctx.paths.params] {
            assert_ne!(got, &p.config);
            assert_ne!(got, &p.vault);
            assert_ne!(got, &p.views);
            assert_ne!(got, &p.params);
        }
        assert!(ctx.paths.config.starts_with(&root), "blocked paths stay inside the broken root");
    }

    #[test]
    fn a_broken_pointer_with_no_readable_root_still_never_targets_the_profile() {
        let ctx = startup_context(Resolution::Broken {
            root: None,
            reason: "ukazatel na pracovní prostor je poškozený: expected a table".to_string(),
        });
        assert!(ctx.blocked.is_some());
        let p = profile_paths();
        assert_ne!(ctx.paths.config, p.config);
        assert_ne!(ctx.paths.vault, p.vault);
        // The sentinel folder does not exist and is never created, so any
        // stray save fails LOUDLY instead of overwriting the profile the
        // user has not chosen (never-destructive rail).
        assert!(!ctx.paths.config.exists());
    }

    #[test]
    fn blocked_paths_never_collide_with_the_profile_dir_itself() {
        let p = blocked_paths(None);
        assert_ne!(p.config.parent(), Some(dbc_state::workspace::profile_dir().as_path()));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui workspace_startup`
Expected: FAIL — `cannot find function startup_context` / `cannot find function blocked_paths`.

- [ ] **Step 3: Implement the pure resolution layer**

Add to `crates/dbc-ui/src/main.rs`, next to the other free functions (near `dialect_for_engine`):

```rust
/// Folder name used for the BLOCKED start's paths when the pointer itself
/// is unreadable, so there is no target folder to name. Deliberately not a
/// real directory: nothing creates it, so every store open and every save
/// against it fails loudly.
const BLOCKED_WORKSPACE_SENTINEL: &str = "__pracovni-prostor-nenalezen__";

/// Paths for a BLOCKED start (design §W4). They point INTO the unusable
/// workspace (or, when the pointer is unreadable, into a sentinel folder
/// that does not exist) — NEVER at the profile's real files. Two reasons,
/// both binding: (a) never a silent fallback — a bug that dismissed the
/// blocking modal must find nothing to connect to; (b) never destructive —
/// an empty default config saved over `%APPDATA%\dbc\config.toml` would
/// erase connections the user never agreed to lose.
pub(crate) fn blocked_paths(root: Option<&Path>) -> dbc_state::workspace::Paths {
    let base = root.map(Path::to_path_buf).unwrap_or_else(|| {
        dbc_state::workspace::profile_dir().join(BLOCKED_WORKSPACE_SENTINEL)
    });
    dbc_state::workspace::workspace_paths(&base)
}

/// What `main()` needs to know before it opens a single store. Pure over
/// `Resolution` (Task 2), so the whole never-silent-fallback rule is
/// testable without a filesystem.
pub(crate) struct StartupContext {
    /// Where every store opens from.
    pub paths: dbc_state::workspace::Paths,
    /// `Some(root)` = workspace mode. Drives the Settings block (Task 5)
    /// and `effective_scripts_root` (Task 7). A BROKEN workspace is NOT an
    /// active workspace, so this stays `None` while `blocked` is `Some`.
    pub workspace_root: Option<PathBuf>,
    /// `Some((root, reason))` ⇒ open `ModalState::WorkspaceMissing` and load
    /// NOTHING (design §W4).
    pub blocked: Option<(Option<PathBuf>, String)>,
}

pub(crate) fn startup_context(res: dbc_state::workspace::Resolution) -> StartupContext {
    match res {
        dbc_state::workspace::Resolution::Profile(paths) => {
            StartupContext { paths, workspace_root: None, blocked: None }
        }
        dbc_state::workspace::Resolution::Workspace { root, paths } => {
            StartupContext { paths, workspace_root: Some(root), blocked: None }
        }
        dbc_state::workspace::Resolution::Broken { root, reason } => StartupContext {
            paths: blocked_paths(root.as_deref()),
            workspace_root: None,
            blocked: Some((root, reason)),
        },
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui workspace_startup`
Expected: PASS (5 tests).

- [ ] **Step 5: Wire `fn main()` — the two call sites the design promised**

Replace the five `dbc_state::default_*_path()` calls at the top of `fn main()` with the resolution. The `AppConfig::load` corrupt-config posture, the history/view-prefs degrade postures, and the status-precedence chain below them are UNCHANGED — only the paths they are handed change, plus the blocked short-circuit:

```rust
    // Design §W0.1: workspace mode is a PATH-RESOLUTION change at exactly
    // two call sites; this is one of them (`dbc-mcp::parse_args` is the
    // other — Task 6). Everything downstream still takes a `&Path`.
    let startup = startup_context(dbc_state::workspace::resolve());
    let config_path = startup.paths.config.clone();
    let vault_path = startup.paths.vault.clone();
    let workspace_root = startup.workspace_root.clone();
    let blocked = startup.blocked.clone();
    // Design §W4: a broken pointer loads NOTHING. Not the workspace's
    // files (they are unusable — that is what "broken" means), and above
    // all not the profile's (that would be the silent fallback this design
    // bans). The modal opened after the window is the only way forward.
    let (config, config_load_error) = if blocked.is_some() {
        (AppConfig::default(), None)
    } else {
        match AppConfig::load(&config_path) {
            Ok(cfg) => (cfg, None),
            Err(e) => (AppConfig::default(), Some(e.to_string())),
        }
    };
    // §W5: history is machine-local in BOTH modes — `workspace_paths`
    // already resolves it to the profile path, so this line is mode-blind.
    let (history, history_open_error) = match HistoryDb::open(&startup.paths.history) {
        Ok(h) => (Some(h), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let (view_prefs, view_prefs_open_error) = if blocked.is_some() {
        (None, None)
    } else {
        match ViewPrefsStore::load(&startup.paths.views) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e.to_string())),
        }
    };
    let param_values = if blocked.is_some() {
        None
    } else {
        ParamValuesStore::load(&startup.paths.params).ok()
    };
```

`Resolution`/`StartupContext` must be `Clone` for the two `.clone()` calls above — `Resolution` already derives it (Task 2); add `#[derive(Clone)]` to `StartupContext` if the compiler asks, or destructure instead. Add `workspace_root` to the `AppView { … }` literal, and to the struct:

```rust
    /// Design §W2: the ACTIVE workspace root, or `None` in profile mode.
    /// There is no third state — a broken pointer never reaches here (it
    /// is blocked at startup, §W4). Written ONLY by `apply_context`.
    workspace_root: Option<PathBuf>,
```

Then, in the existing post-`open_window` `window_handle.update(cx, …)` block, after the sidebar seeding:

```rust
            // Design §W4: the blocking modal goes up LAST, so it occludes a
            // fully-constructed (and deliberately empty) app.
            if let Some((root, reason)) = blocked {
                view.open_workspace_missing_modal(root, reason, cx);
            }
```

`blocked` must be moved into the `application().run(move |cx| …)` closure alongside `config_path`/`vault_path` — the closure is already `move`.

- [ ] **Step 6: The blocking modal — state, policy, render**

In `crates/dbc-ui/src/connections_ui.rs`, add the `ModalState` arm:

```rust
    /// Design §W4: the pointer file names a workspace this build cannot
    /// use (folder missing, marker gone, unreadable, future format). The
    /// app is already up but EMPTY — no config, no vault, no view prefs
    /// were loaded — and this modal is the only way out. Deliberately the
    /// most locked-down arm in this enum: Enter is `Ignore`, Esc does not
    /// close it (see `AppView::on_cancel_query`), and its three buttons are
    /// the three explicit choices the design enumerates. `root` is `None`
    /// only when the POINTER itself was unparsable (there is no folder to
    /// name); `error` carries a failed re-pick's message, shown in place.
    WorkspaceMissing { root: Option<std::path::PathBuf>, reason: String, error: Option<String> },
```

Policy table (`modal_confirm_kind`) — add it to the `Ignore` group, with its own reason:

```rust
        // §W4: no Enter shortcut past a wrong-context guard. Each of the
        // three choices (re-pick / explicit profile / quit) must be a
        // deliberate click.
        | ModalState::WorkspaceMissing { .. } => ModalConfirmKind::Ignore,
```

Esc allow-list in `AppView::on_cancel_query` (main.rs) — the arm that makes it BLOCKING:

```rust
                // §W4: not Esc-closable, ever. Dismissing it would leave an
                // app with an empty config and no context — the modal IS
                // the recovery UI, not an interruption of one.
                connections_ui::ModalState::WorkspaceMissing { .. } => false,
```

Render helper in `connections_ui.rs` (called from `render_modal_overlay`'s match):

```rust
fn render_workspace_missing_panel(
    root: &Option<std::path::PathBuf>,
    reason: &str,
    error: &Option<String>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let path_line = root
        .as_ref()
        .map(|r| r.display().to_string())
        .unwrap_or_else(|| "ukazatel na pracovní prostor je nečitelný".to_string());
    let button = |id: &'static str, label: &'static str, cx: &mut Context<AppView>| {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(cx.theme().bg_hover)
            .cursor_pointer()
            .child(label)
    };
    let mut panel = div()
        .id("workspace-missing-panel")
        .w(px(460.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Pracovní prostor nenalezen"))
        .child(div().text_color(cx.theme().text_muted).child(path_line))
        .child(div().text_color(cx.theme().danger).child(format!("error: {reason}")));
    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(format!("error: {e}")));
    }
    panel
        .child(
            button("workspace-missing-find", "Najít složku…", cx)
                .on_click(cx.listener(|this, _, _, cx| this.pick_workspace_for_recovery(cx))),
        )
        .child(div().text_color(cx.theme().text_muted).child(
            "Otevře se lokální profil — jiná připojení a nastavení než v pracovním prostoru.",
        ))
        .child(
            button("workspace-missing-profile", "Použít lokální profil", cx)
                .on_click(cx.listener(|this, _, _, cx| this.use_local_profile(cx))),
        )
        .child(
            button("workspace-missing-quit", "Ukončit", cx)
                .on_click(cx.listener(|_this, _, _, cx| cx.quit())),
        )
        .into_any_element()
}
```

and in `render_modal_overlay`:

```rust
            ModalState::WorkspaceMissing { root, reason, error } => {
                render_workspace_missing_panel(&root, &reason, &error, cx)
            }
```

- [ ] **Step 7: The three choices + the ONE context swap**

In `main.rs`:

```rust
    /// Design §W4. Opened only from `main()`'s startup wiring — there is no
    /// other way to reach a broken resolution, and no guard is needed
    /// (nothing else can be open one frame after the window appears).
    fn open_workspace_missing_modal(
        &mut self,
        root: Option<PathBuf>,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.modal =
            Some(connections_ui::ModalState::WorkspaceMissing { root, reason, error: None });
        // UX-polish §1.4: no-input modal, cx-only opener.
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// „Najít složku…" — THE WORKSPACE MOVED, not "make a new one": only a
    /// folder carrying a valid `dbc-workspace.toml` marker is accepted here
    /// (design §W4). An empty folder is refused with the same honesty as a
    /// non-workspace one — initialization is a Settings decision with its
    /// own confirm + security warning (Task 5), never a recovery side
    /// effect.
    fn pick_workspace_for_recovery(&mut self, cx: &mut Context<Self>) {
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Otevřít".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => return,           // cancelled: the modal stays up
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_missing_error(format!("dialog selhal: {e}"), cx);
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_missing_error("dialog není dostupný".into(), cx);
                    });
                    return;
                }
            };
            // Off the UI thread: classification + the pointer write.
            let outcome: Result<PathBuf, String> = cx
                .background_spawn(async move {
                    match dbc_state::workspace::classify(&picked) {
                        dbc_state::workspace::Classification::Workspace => {
                            dbc_state::workspace::write_pointer(
                                &dbc_state::workspace::pointer_path(),
                                &picked,
                            )
                            .map_err(|e| e.message)?;
                            Ok(picked)
                        }
                        dbc_state::workspace::Classification::FutureFormat(f) => Err(format!(
                            "pracovní prostor vyžaduje novější verzi aplikace (formát {f})"
                        )),
                        dbc_state::workspace::Classification::Unreadable(m) => Err(m),
                        _ => Err(
                            "vybraná složka není pracovní prostor dbc — vyberte složku s dbc-workspace.toml"
                                .to_string(),
                        ),
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| match outcome {
                Ok(root) => {
                    view.close_modal(cx);
                    view.apply_context(Some(root), cx);
                }
                Err(e) => view.set_workspace_missing_error(e, cx),
            });
        })
        .detach();
    }

    fn set_workspace_missing_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::WorkspaceMissing { error, .. }) = &mut self.modal {
            *error = Some(message);
        }
        cx.notify();
    }

    /// „Použít lokální profil" — the EXPLICIT user action design §W4
    /// contrasts with a silent fallback: it deletes the pointer (so the
    /// next start is plain profile mode) and swaps the live context. The
    /// workspace folder itself is not touched in any way.
    fn use_local_profile(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = dbc_state::workspace::clear_pointer(&dbc_state::workspace::pointer_path()) {
            self.set_workspace_missing_error(e.message, cx);
            return;
        }
        self.close_modal(cx);
        self.apply_context(None, cx);
    }
```

And the swap itself — **the one seam** (design §W3.4). Task 5 wraps it with gates and confirm modals; Task 7 adds a single scripts line to its body; nobody writes a second one:

```rust
    /// Design §W3.4 — the live, in-place context swap. THE single seam:
    /// „Najít složku…" (§W4), „Použít lokální profil" (§W4), init (§W3.2),
    /// adopt (§W3.3) and „Přejít na lokální profil" all end here.
    ///
    /// PRECONDITIONS the caller owns: the §W3.1 gates have passed (no run
    /// in flight, no pending apply/discard, no dirty script — Task 5's
    /// `context_switch_blocked`) and the pointer file has already been
    /// written or cleared. This fn performs no I/O beyond loading the NEW
    /// context's stores, and never deletes, moves, or rewrites anything in
    /// the OLD one (never-destructive rail).
    ///
    /// `AppConfig::load` runs on the UI thread here, exactly as it does in
    /// `fn main()` — a small TOML read, deliberately synchronous so the
    /// swap is atomic from the user's point of view (no frame in which the
    /// paths are new but the connections are still the old ones).
    pub(crate) fn apply_context(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        let paths = match &root {
            Some(r) => dbc_state::workspace::workspace_paths(r),
            None => dbc_state::workspace::profile_paths(),
        };
        // §W3.1: the connection list itself is about to change — keeping a
        // session from the OLD context alive under the NEW config is
        // exactly the silent context mixing this design bans.
        self.clear_active_connection(cx);
        // §W3.4: a workspace vault is a DIFFERENT file; the session unlock
        // must not carry over. The existing lazy prompt re-fires on the
        // next secret use, at most once per run.
        self.vault = None;
        self.config_path = paths.config.clone();
        self.vault_path = paths.vault.clone();
        let (config, config_load_error) = match AppConfig::load(&paths.config) {
            Ok(c) => (c, None),
            Err(e) => (AppConfig::default(), Some(e.to_string())),
        };
        self.config = config;
        self.config_load_error = config_load_error;
        // Existing degrade-to-None postures, unchanged.
        self.view_prefs = ViewPrefsStore::load(&paths.views).ok();
        self.param_values = ParamValuesStore::load(&paths.params).ok();
        // history: NOT touched — machine-local in both modes (§W5).
        self.workspace_root = root.clone();
        self.refresh_grouped_cache(cx);
        self.refresh_tree_context(cx);
        // Task 7 adds ONE line here: `self.tree.update(cx, |t, cx|
        // t.reset_scripts(cx)); self.start_scripts_scan(cx);` — the scripts
        // root just changed. It cannot land now: the tree's scripts API is
        // dark until the flip.
        self.status = match &root {
            Some(r) => format!("pracovní prostor: {}", r.display()),
            None => "lokální profil obnoven".to_string(),
        };
        if let Some(detail) = self.config_load_error.clone() {
            self.status = format!("error: config.toml je poškozený – oprav nebo smaž soubor ({detail})");
        }
        cx.notify();
    }

    /// §W3.1's „Aktivní připojení bude odpojeno." made real. The app keeps
    /// no persistent session (the runner is per-operation — sidebar design
    /// fact 0.1), so disconnecting IS dropping the active identity and
    /// bumping `switch_generation` so an in-flight switch's result can
    /// never land in the NEW context. The CLI-arg root goes too: it belongs
    /// to the old context and, per the sidebar design, cannot come back.
    fn clear_active_connection(&mut self, cx: &mut Context<Self>) {
        self.active_connection_id = None;
        self.active_database = None;
        self.conn_url = None;
        self.switch_generation = self.switch_generation.wrapping_add(1);
        self.dropdown_open = false;
        cx.notify();
    }
```

- [ ] **Step 8: Manual verification of the blocking posture**

Build and run: `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`, having first hand-written `%APPDATA%\dbc\workspace.toml` with `path = "D:\\neexistuje"`.
Expected, all four:
1. The modal reads „Pracovní prostor nenalezen", the path, and „error: složka neexistuje".
2. Esc does nothing; Enter does nothing.
3. The connection dropdown behind it lists NOTHING (empty config).
4. „Použít lokální profil" deletes the pointer, restores the real connection list, and the status reads „lokální profil obnoven"; `%APPDATA%\dbc\config.toml` is byte-identical to before the run (`certutil -hashfile` before/after).

- [ ] **Step 9: Gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings.

- [ ] **Step 10: Commit**

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: startup workspace resolution + blocking WorkspaceMissing modal + apply_context swap (workspace T4)"
```

---

### Task 5: Settings „Pracovní prostor" block + init/adopt confirm modals + the gated live swap

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`WORKSPACE_GIT_WARNING`, `ModalState::WorkspaceConfirm`, `WorkspaceConfirmMode`, `modal_confirm_kind`, `render_settings_panel`, `render_workspace_confirm_panel`, `render_modal_overlay`)
- Modify: `crates/dbc-ui/src/main.rs` (`context_switch_blocked`, `start_workspace_pick`, `start_leave_workspace`, `confirm_workspace`, Esc allow-list arm)

Batch 3 lane A — runs in PARALLEL with Task 6 (`dbc-mcp`, a different crate).

**Interfaces:**
- Consumes: `AppView::apply_context(Option<PathBuf>, &mut Context<Self>)` and `AppView.workspace_root` (Task 4, exact signatures); `dbc_state::workspace::{classify, Classification, init_workspace, profile_paths, pointer_path, write_pointer, clear_pointer, MARKER_FILE, SCRIPTS_SUBDIR}` (Task 2, exact names).
- Produces (Task 7/8 consume these):
  - `pub(crate) const connections_ui::WORKSPACE_GIT_WARNING: &str`
  - `pub(crate) enum connections_ui::WorkspaceConfirmMode { Init, Adopt, ToProfile }` (`Debug + Clone + Copy + PartialEq + Eq`)
  - `connections_ui::ModalState::WorkspaceConfirm { mode: WorkspaceConfirmMode, root: Option<PathBuf>, error: Option<String>, running: bool }`
  - `pub(crate) fn AppView::context_switch_blocked(&self) -> Option<String>` — **Task 8 adds its dirty-script arm to THIS function; it does not write a second gate.**

**Ordering note (recorded, not a stub):** design §W3.1 lists „the Part S §5.5 dirty script guard runs first" among the gates. `AppView.script_binding` does not exist until Task 8, so `context_switch_blocked` ships here complete for the state that exists, and **Task 8 Step 6 adds the `script_binding` arm to this same function**. That is an extension of a real gate, not a placeholder: nothing here returns a hard-coded `None` standing in for a check.

- [ ] **Step 1: Write the failing tests**

Add to `crates/dbc-ui/src/connections_ui.rs`'s test module (pure — no GPUI, no fs):

```rust
    /// Deliverable copy (design §W6.3): the honest warning, byte for byte.
    /// It names the permanence of git history, the vault file by name, the
    /// master-password dependency, and BOTH mitigations. If a future edit
    /// softens any of those four, this test is the thing that must be
    /// argued with.
    #[test]
    fn workspace_git_warning_is_byte_pinned() {
        assert_eq!(
            WORKSPACE_GIT_WARNING,
            "Upozornění: složku verzujete sami — git zůstává zcela mimo aplikaci. \
             Historie gitu je trvalá: jednou commitnutý trezor (vault.bin) z ní nelze \
             nikdy spolehlivě odstranit. Bezpečnost celé složky se pak rovná síle vašeho \
             master hesla. Repozitář držte privátní, nebo vault.bin vyřaďte z verzování \
             (.gitignore ve složce má připravený zakomentovaný řádek)."
        );
    }

    #[test]
    fn the_warning_carries_no_secret_and_no_git_command() {
        // Security rail (§W6.5) + the permanent no-git rail (§W6.4): this
        // string is shown, never executed, and must never grow a recipe.
        for banned in ["git ", "http", "password=", "heslo:"] {
            assert!(!WORKSPACE_GIT_WARNING.contains(banned), "warning must not contain {banned:?}");
        }
    }

    #[test]
    fn workspace_confirm_titles_and_buttons_are_the_designed_copy() {
        assert_eq!(workspace_confirm_title(WorkspaceConfirmMode::Init), "Vytvořit pracovní prostor");
        assert_eq!(workspace_confirm_title(WorkspaceConfirmMode::Adopt), "Otevřít pracovní prostor");
        assert_eq!(
            workspace_confirm_title(WorkspaceConfirmMode::ToProfile),
            "Přejít na lokální profil"
        );
        assert_eq!(workspace_confirm_button(WorkspaceConfirmMode::Init), "Rozumím, vytvořit");
        assert_eq!(workspace_confirm_button(WorkspaceConfirmMode::Adopt), "Otevřít");
        assert_eq!(workspace_confirm_button(WorkspaceConfirmMode::ToProfile), "Přejít");
    }

    #[test]
    fn every_workspace_confirm_mode_warns_about_the_disconnect() {
        for mode in [
            WorkspaceConfirmMode::Init,
            WorkspaceConfirmMode::Adopt,
            WorkspaceConfirmMode::ToProfile,
        ] {
            assert!(workspace_confirm_lines(mode)
                .iter()
                .any(|l| l == "Aktivní připojení bude odpojeno."));
        }
        // §W3.3: adopt additionally explains the foreign vault.
        assert!(workspace_confirm_lines(WorkspaceConfirmMode::Adopt)
            .iter()
            .any(|l| l == "Trezor tohoto prostoru se odemyká jeho vlastním master heslem."));
        // §W6.3: the git warning renders on the two folder-facing modes;
        // going back to the profile writes nothing into any folder.
        assert!(workspace_confirm_lines(WorkspaceConfirmMode::Init)
            .iter()
            .any(|l| l == WORKSPACE_GIT_WARNING));
        assert!(workspace_confirm_lines(WorkspaceConfirmMode::Adopt)
            .iter()
            .any(|l| l == WORKSPACE_GIT_WARNING));
        assert!(!workspace_confirm_lines(WorkspaceConfirmMode::ToProfile)
            .iter()
            .any(|l| l == WORKSPACE_GIT_WARNING));
    }

    #[test]
    fn enter_is_inert_on_every_new_workspace_modal() {
        // §W3.2/§W4: the BUTTON is the gate — a deliberate,
        // security-relevant decision, same posture as ScriptRun.
        assert_eq!(
            modal_confirm_kind(&ModalState::WorkspaceMissing {
                root: None,
                reason: String::new(),
                error: None,
            }),
            ModalConfirmKind::Ignore
        );
        assert_eq!(
            modal_confirm_kind(&ModalState::WorkspaceConfirm {
                mode: WorkspaceConfirmMode::Init,
                root: Some(std::path::PathBuf::from("D:\\ws")),
                error: None,
                running: false,
            }),
            ModalConfirmKind::Ignore
        );
    }

    #[test]
    fn a_picked_folder_is_classified_into_exactly_one_outcome() {
        use dbc_state::workspace::Classification;
        assert_eq!(
            workspace_pick_outcome(Classification::Workspace),
            Ok(WorkspaceConfirmMode::Adopt)
        );
        assert_eq!(workspace_pick_outcome(Classification::Empty), Ok(WorkspaceConfirmMode::Init));
        assert_eq!(
            workspace_pick_outcome(Classification::NonEmpty),
            Err("složka není pracovní prostor dbc a není prázdná — vyberte prázdnou složku nebo existující pracovní prostor".to_string())
        );
        assert_eq!(
            workspace_pick_outcome(Classification::FutureFormat(7)),
            Err("pracovní prostor vyžaduje novější verzi aplikace (formát 7)".to_string())
        );
        assert_eq!(
            workspace_pick_outcome(Classification::Unreadable("přístup odepřen".into())),
            Err("přístup odepřen".to_string())
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui workspace_git_warning`
Expected: FAIL — `cannot find value WORKSPACE_GIT_WARNING`, `cannot find function workspace_confirm_title`.

- [ ] **Step 3: The copy, the mode, the pure helpers**

In `crates/dbc-ui/src/connections_ui.rs`:

```rust
/// Design §W6.3 — the honest in-app warning, shown inside the init/adopt
/// confirm modals AND statically in the Settings „Pracovní prostor" block
/// while the folder-pick flow is offered. Once per decision point, never on
/// startup: the user made this call knowingly (they chose „Šifrovaný trezor
/// do složky" over the never-version-secrets recommendation), so the
/// warning exists to keep the call INFORMED, not to relitigate it.
pub(crate) const WORKSPACE_GIT_WARNING: &str = "Upozornění: složku verzujete sami — git zůstává zcela mimo aplikaci. \
Historie gitu je trvalá: jednou commitnutý trezor (vault.bin) z ní nelze \
nikdy spolehlivě odstranit. Bezpečnost celé složky se pak rovná síle vašeho \
master hesla. Repozitář držte privátní, nebo vault.bin vyřaďte z verzování \
(.gitignore ve složce má připravený zakomentovaný řádek).";

/// Which context change a `ModalState::WorkspaceConfirm` is confirming.
/// ONE modal for all three (design §W3.2/§W3.3/§W3.4) so the gates, the
/// „Aktivní připojení bude odpojeno." line and the Enter-inert policy are
/// written exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceConfirmMode {
    /// Empty folder ⇒ copy + `scripts/` + `.gitignore` + marker (§W3.2).
    Init,
    /// Marker present ⇒ pointer only, no files written (§W3.3).
    Adopt,
    /// „Přejít na lokální profil" ⇒ delete the pointer, touch nothing in
    /// the folder (§W3.4's reverse switch).
    ToProfile,
}

pub(crate) fn workspace_confirm_title(mode: WorkspaceConfirmMode) -> &'static str {
    match mode {
        WorkspaceConfirmMode::Init => "Vytvořit pracovní prostor",
        WorkspaceConfirmMode::Adopt => "Otevřít pracovní prostor",
        WorkspaceConfirmMode::ToProfile => "Přejít na lokální profil",
    }
}

pub(crate) fn workspace_confirm_button(mode: WorkspaceConfirmMode) -> &'static str {
    match mode {
        WorkspaceConfirmMode::Init => "Rozumím, vytvořit",
        WorkspaceConfirmMode::Adopt => "Otevřít",
        WorkspaceConfirmMode::ToProfile => "Přejít",
    }
}

/// The body lines of the confirm modal, in render order — pure, so the
/// deliverable copy is testable without GPUI.
pub(crate) fn workspace_confirm_lines(mode: WorkspaceConfirmMode) -> Vec<&'static str> {
    let mut lines = vec!["Aktivní připojení bude odpojeno."];
    match mode {
        WorkspaceConfirmMode::Init => {
            lines.push("Nastavení, připojení a trezor se do složky ZKOPÍRUJÍ; původní soubory zůstanou beze změny.");
            lines.push(WORKSPACE_GIT_WARNING);
        }
        WorkspaceConfirmMode::Adopt => {
            lines.push("Trezor tohoto prostoru se odemyká jeho vlastním master heslem.");
            lines.push(WORKSPACE_GIT_WARNING);
        }
        WorkspaceConfirmMode::ToProfile => {
            lines.push("Soubory v pracovním prostoru zůstanou beze změny.");
        }
    }
    lines
}

/// Design §W3's folder classification, mapped to a decision. Pure over
/// `Classification` so all five outcomes are tested without a filesystem.
pub(crate) fn workspace_pick_outcome(
    c: dbc_state::workspace::Classification,
) -> Result<WorkspaceConfirmMode, String> {
    use dbc_state::workspace::Classification as C;
    match c {
        C::Workspace => Ok(WorkspaceConfirmMode::Adopt),
        C::Empty => Ok(WorkspaceConfirmMode::Init),
        // Never scatter app files into someone's Documents folder by
        // misclick; never adopt a folder we cannot vouch for (§W3 case 3).
        C::NonEmpty => Err(
            "složka není pracovní prostor dbc a není prázdná — vyberte prázdnou složku nebo existující pracovní prostor"
                .to_string(),
        ),
        C::FutureFormat(f) => {
            Err(format!("pracovní prostor vyžaduje novější verzi aplikace (formát {f})"))
        }
        C::Unreadable(m) => Err(m),
    }
}
```

Add the modal arm:

```rust
    /// Design §W3.2/§W3.3/§W3.4: the ONE confirm gate in front of a context
    /// change. `root` is the picked folder (`None` only for `ToProfile`).
    /// `running` holds the modal mutated in place for the duration of the
    /// background init/pointer write — the `AnalyzeWriteConfirm`/
    /// `BackupRestore` posture, so the app-wide `self.modal.is_some()`
    /// busy-guards keep holding and Esc cannot abandon a half-done init.
    /// Enter is INERT (`modal_confirm_kind`): the button is the gate.
    WorkspaceConfirm {
        mode: WorkspaceConfirmMode,
        root: Option<std::path::PathBuf>,
        error: Option<String>,
        running: bool,
    },
```

`modal_confirm_kind` — extend the `Ignore` group (alongside `WorkspaceMissing` from Task 4):

```rust
        | ModalState::WorkspaceConfirm { .. } => ModalConfirmKind::Ignore,
```

Esc allow-list in `main.rs::on_cancel_query`:

```rust
                // §W3.2: nothing is dispatched until the button is clicked,
                // and nothing secret is typed here — so Esc cancels freely,
                // BUT never mid-init (`running`), same reasoning as
                // `BackupRestore`'s `!session.is_running()` above.
                connections_ui::ModalState::WorkspaceConfirm { running, .. } => !running,
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui workspace`
Expected: PASS (6 new tests + Task 4's 5 startup tests).

- [ ] **Step 5: The gate (§W3.1)**

In `main.rs`:

```rust
    /// Design §W3.1 — the common gate in front of EVERY context change
    /// (init, adopt, „Přejít na lokální profil"). A context replacement
    /// demands a quiet app: the same gate style as `start_script_pick`.
    /// Returns the Czech refusal to show, or `None` to proceed.
    ///
    /// EXTENSION POINT: Task 8 adds the dirty-`script_binding` arm here
    /// (Part S §5.5's guard) once that field exists. There must be exactly
    /// ONE gate function — a second „is it safe to switch" predicate is a
    /// review-blocking defect.
    pub(crate) fn context_switch_blocked(&self) -> Option<String> {
        if self.cancel.is_some() {
            return Some("nejprve dokončete běžící dotaz".to_string());
        }
        if self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return Some("nejprve dokončete rozpracované úpravy".to_string());
        }
        // The Settings modal itself is the caller's own modal — every other
        // open dialog blocks (single-modal invariant, app-wide).
        if !matches!(self.modal, None | Some(connections_ui::ModalState::Settings)) {
            return Some("nejprve zavřete otevřený dialog".to_string());
        }
        None
    }
```

- [ ] **Step 6: The Settings block**

In `render_settings_panel`, insert a „Pracovní prostor" block under the „Motiv" radios (above where Task 7 will put „Složka skriptů").

First, restructure the existing single chained expression into a rebindable local — the block below needs a conditional child, which a `div().child(..).child(..)` chain cannot express. Split the current body exactly at the theme radios:

```rust
        let mut panel = div()
            .id("settings-panel")
            .w(px(360.))
            .bg(cx.theme().bg_panel)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .text_color(cx.theme().text_primary)
            .child(div().text_size(px(16.)).child("Nastavení"))
            .child(div().text_color(cx.theme().text_muted).child("Motiv"))
            .child(radio("settings-theme-dark", "Tmavý", dbc_state::ThemeMode::Dark, mode, cx))
            .child(radio("settings-theme-light", "Světlý", dbc_state::ThemeMode::Light, mode, cx));
```

Then the new block:

```rust
        let ws_root = self.workspace_root.clone();
        panel = panel.child(div().text_color(cx.theme().text_muted).child("Pracovní prostor"));
        match &ws_root {
            Some(root) => {
                // Workspace mode (§W3): NO folder picker here — creating a
                // second workspace from inside one is not a flow this
                // design has (§W3.2 copies from the PROFILE); the way out
                // is the profile and back.
                panel = panel
                    .child(div().child(format!("Pracovní prostor: {}", root.display())))
                    .child(
                        div()
                            .id("settings-workspace-leave")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Přejít na lokální profil")
                            .on_click(cx.listener(|this, _, _, cx| this.start_leave_workspace(cx))),
                    );
            }
            None => {
                panel = panel
                    .child(div().text_color(cx.theme().text_muted).child(format!(
                        "Lokální profil ({})",
                        dbc_state::workspace::profile_dir().display()
                    )))
                    .child(
                        div()
                            .id("settings-workspace-pick")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Použít složku…")
                            .on_click(cx.listener(|this, _, _, cx| this.start_workspace_pick(cx))),
                    )
                    // §W6.3(b): the warning renders STATICALLY wherever the
                    // folder-pick flow is offered — not only inside the
                    // confirm modal.
                    .child(
                        div()
                            .text_color(cx.theme().text_muted)
                            .child(WORKSPACE_GIT_WARNING),
                    );
            }
        }
```

Keep the existing „Zavřít" button LAST, and end the fn with `panel.into_any_element()`.

- [ ] **Step 7: Pick → classify → confirm**

In `main.rs`:

```rust
    /// §W3: „Použít složku…". Gates first, then picks, then classifies in
    /// the background, then opens the confirm modal. NOTHING is written
    /// before the user clicks the confirm button.
    fn start_workspace_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.context_switch_blocked() {
            self.status = format!("error: {reason}");
            cx.notify();
            return;
        }
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Použít".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "výběr zrušen".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog selhal: {e}");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let probe = picked.clone();
            let outcome = cx
                .background_spawn(async move {
                    connections_ui::workspace_pick_outcome(dbc_state::workspace::classify(&probe))
                })
                .await;
            let _ = this.update(cx, |view, cx| match outcome {
                Ok(mode) => view.open_workspace_confirm(mode, Some(picked), cx),
                Err(e) => {
                    view.status = format!("error: {e}");
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// §W3.4's reverse switch — same gate, same confirm shape.
    fn start_leave_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.context_switch_blocked() {
            self.status = format!("error: {reason}");
            cx.notify();
            return;
        }
        self.open_workspace_confirm(connections_ui::WorkspaceConfirmMode::ToProfile, None, cx);
    }

    fn open_workspace_confirm(
        &mut self,
        mode: connections_ui::WorkspaceConfirmMode,
        root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // The Settings modal is what the user clicked from — replace it in
        // place (the single-modal invariant holds: exactly one is open).
        self.modal = Some(connections_ui::ModalState::WorkspaceConfirm {
            mode,
            root,
            error: None,
            running: false,
        });
        self.modal_needs_focus = true;
        cx.notify();
    }
```

- [ ] **Step 8: Confirm — the write, then the swap**

```rust
    /// The confirm button of `ModalState::WorkspaceConfirm`. Order is the
    /// design's, and the order matters: files first, marker last (inside
    /// `init_workspace`), pointer only after that returns `Ok`, live swap
    /// only after the pointer is on disk. A failure at any step leaves the
    /// PREVIOUS context fully intact and the error in the modal.
    fn confirm_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::WorkspaceConfirm { mode, root, running, .. }) =
            &mut self.modal
        else {
            return;
        };
        if *running {
            return; // double-click guard, `KillConfirm::dispatched`'s role
        }
        let (mode, root) = (*mode, root.clone());
        // Re-run the gate: the pick + classification did not block the app.
        if let Some(reason) = self.context_switch_blocked() {
            self.set_workspace_confirm_error(reason, cx);
            return;
        }
        if let Some(connections_ui::ModalState::WorkspaceConfirm { running, .. }) = &mut self.modal
        {
            *running = true;
        }
        cx.notify();
        // §W3.2 step 1: init copies from the PROFILE, always — workspace
        // mode offers no picker (§W3/Step 6), so the profile is the only
        // possible origin. `from` is captured here, on the UI thread, and
        // moved into the background job.
        let from = dbc_state::workspace::profile_paths();
        let pointer = dbc_state::workspace::pointer_path();
        cx.spawn(async move |this, cx| {
            let job_root = root.clone();
            let result: Result<(), String> = cx
                .background_spawn(async move {
                    match mode {
                        connections_ui::WorkspaceConfirmMode::Init => {
                            let root = job_root.clone().ok_or("chybí cílová složka")?;
                            // Copies + scripts/ + .gitignore + MARKER LAST.
                            // Every write inside goes through the shared
                            // rails (`fsutil::write_atomic` /
                            // `join_component` / `entry_exists_ci`) — this
                            // call site must NEVER grow its own copy loop.
                            dbc_state::workspace::init_workspace(&root, &from)
                                .map_err(|e| e.message)?;
                            dbc_state::workspace::write_pointer(&pointer, &root)
                                .map_err(|e| e.message)
                        }
                        connections_ui::WorkspaceConfirmMode::Adopt => {
                            let root = job_root.clone().ok_or("chybí cílová složka")?;
                            // §W3.3: NOTHING is written but the pointer.
                            dbc_state::workspace::write_pointer(&pointer, &root)
                                .map_err(|e| e.message)
                        }
                        connections_ui::WorkspaceConfirmMode::ToProfile => {
                            dbc_state::workspace::clear_pointer(&pointer).map_err(|e| e.message)
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(()) => {
                    view.close_modal(cx);
                    view.apply_context(root.clone(), cx);
                }
                Err(e) => {
                    if let Some(connections_ui::ModalState::WorkspaceConfirm {
                        running, error, ..
                    }) = &mut view.modal
                    {
                        *running = false;
                        *error = Some(e);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_workspace_confirm_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::WorkspaceConfirm { error, running, .. }) =
            &mut self.modal
        {
            *running = false;
            *error = Some(message);
        }
        cx.notify();
    }
```

Render (`connections_ui.rs`), wired into `render_modal_overlay`'s match:

```rust
fn render_workspace_confirm_panel(
    mode: WorkspaceConfirmMode,
    root: &Option<std::path::PathBuf>,
    error: &Option<String>,
    running: bool,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let mut panel = div()
        .id("workspace-confirm-panel")
        .w(px(460.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child(workspace_confirm_title(mode)));
    if let Some(r) = root {
        panel = panel.child(div().text_color(cx.theme().text_muted).child(r.display().to_string()));
    }
    for line in workspace_confirm_lines(mode) {
        panel = panel.child(div().text_color(cx.theme().text_muted).child(line));
    }
    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(format!("error: {e}")));
    }
    let confirm_bg = if running { cx.theme().bg_selected } else { cx.theme().bg_hover };
    panel
        .child(
            div()
                .id("workspace-confirm-ok")
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(confirm_bg)
                .cursor_pointer()
                .child(if running { "Pracuji…" } else { workspace_confirm_button(mode) })
                .on_click(cx.listener(|this, _, _, cx| this.confirm_workspace(cx))),
        )
        .child(
            div()
                .id("workspace-confirm-cancel")
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(cx.theme().bg_hover)
                .cursor_pointer()
                .child("Zrušit")
                .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
        )
        .into_any_element()
}
```

Decided, not overlooked: „Zrušit" closes the modal outright rather than returning to „Nastavení". The single-modal invariant means the confirm REPLACED the settings panel, and re-opening Settings is one click on the topbar gear — a modal stack is not a shape this app has anywhere.

- [ ] **Step 9: Manual verification of the two safety properties**

Build and run `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`, then, with a real `%APPDATA%\dbc\config.toml` and `vault.bin` in place:
1. Nastavení → „Použít složku…" → pick a NON-empty folder (e.g. `Documents`). Expected: refusal „error: složka není pracovní prostor dbc a není prázdná — …" and **not one file created** in it (`dir /a` before/after).
2. Pick a fresh empty folder → the modal shows the path, the copy line, „Aktivní připojení bude odpojeno." and the full §W6.3 warning → „Rozumím, vytvořit". Expected: `dbc-workspace.toml`, `config.toml`, `vault.bin`, `views.toml`, `params.toml`, `scripts\`, `.gitignore` exist; the profile files are byte-identical (`certutil -hashfile` before/after); the status reads „pracovní prostor: {path}"; the connection dropdown shows the same connections; the first connect re-prompts for the master password (the vault session did not carry over).
3. Nastavení → „Přejít na lokální profil" → „Přejít". Expected: the workspace folder is untouched, `%APPDATA%\dbc\workspace.toml` is gone, status „lokální profil obnoven".
4. `findstr /S /I "SUPERTAJNE" <workspace>\*.toml <workspace>\.gitignore` (using a real password you saved) finds NOTHING — the §W6.5 rail, verified by hand once.

- [ ] **Step 10: Gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings.

- [ ] **Step 11: Commit**

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: Settings workspace block + init/adopt/leave confirm modals + gated context swap (workspace T5)"
```

---

### Task 6: `dbc-mcp` pointer-file support — same resolution rule, same fail-loud posture

**Files:**
- Modify: `crates/dbc-mcp/src/main.rs` (`Command`, `Args`, `parse_args` → pure `parse_args_from`, `print_usage`, `main`)

Batch 3 lane B — runs in PARALLEL with Task 5 (different crate, no shared file). This is the SECOND (and last) of the two `default_*_path()` call sites design §W0.1 names; after this task, nothing in the workspace resolves paths on its own.

**Interfaces:**
- Consumes: `dbc_state::workspace::{Resolution, resolve, profile_paths, workspace_paths}` (Task 2, exact names).
- Produces:
  - `enum Command { Serve, Setup { remove: bool }, Help, Fail(String) }`
  - `fn parse_args_from(raw: &[String], res: dbc_state::workspace::Resolution) -> Args` (pure — no env, no fs)
  - `fn workspace_broken_message(root: &Option<PathBuf>, reason: &str) -> String` (pure)

**The rule (design §W7, binding):** default paths follow the SAME pointer file the GUI reads. Explicit `--config`/`--vault` still win. A broken pointer makes dbc-mcp **exit with the error** — it must not silently serve profile-mode connections either. Precision the design leaves to the plan and this task pins: a broken pointer is fatal only when its defaults would actually be USED — `--help` needs neither path, `setup` needs only the vault, `serve` needs both. Overriding exactly what you need is legitimate and must keep working.

- [ ] **Step 1: Write the failing tests**

Add to `crates/dbc-mcp/src/main.rs`:

```rust
#[cfg(test)]
mod parse_args_tests {
    use super::*;
    use dbc_state::workspace::{profile_paths, workspace_paths, Resolution};

    fn raw(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    fn profile() -> Resolution {
        Resolution::Profile(profile_paths())
    }

    fn workspace() -> Resolution {
        let root = PathBuf::from("D:\\ws");
        Resolution::Workspace { root: root.clone(), paths: workspace_paths(&root) }
    }

    fn broken() -> Resolution {
        Resolution::Broken {
            root: Some(PathBuf::from("D:\\ws-gone")),
            reason: "složka neexistuje".to_string(),
        }
    }

    #[test]
    fn no_pointer_keeps_todays_defaults_exactly() {
        let a = parse_args_from(&raw(&[]), profile());
        assert_eq!(a.config, dbc_state::default_config_path());
        assert_eq!(a.vault, dbc_state::default_vault_path());
        assert!(matches!(a.command, Command::Serve));
    }

    #[test]
    fn a_valid_pointer_moves_both_defaults_into_the_workspace() {
        let a = parse_args_from(&raw(&[]), workspace());
        assert_eq!(a.config, PathBuf::from("D:\\ws").join("config.toml"));
        assert_eq!(a.vault, PathBuf::from("D:\\ws").join("vault.bin"));
    }

    #[test]
    fn explicit_flags_still_win_over_the_workspace() {
        let a = parse_args_from(&raw(&["--config", "C:\\x.toml", "--vault", "C:\\x.bin"]), workspace());
        assert_eq!(a.config, PathBuf::from("C:\\x.toml"));
        assert_eq!(a.vault, PathBuf::from("C:\\x.bin"));
        assert!(matches!(a.command, Command::Serve));
    }

    #[test]
    fn a_broken_pointer_fails_loudly_instead_of_serving_the_profile() {
        // The §W4/§W7 rail on the MCP side: no silent profile fallback.
        let a = parse_args_from(&raw(&[]), broken());
        let Command::Fail(msg) = a.command else { panic!("expected Fail") };
        assert!(msg.contains("D:\\ws-gone"), "names the folder: {msg}");
        assert!(msg.contains("složka neexistuje"), "names the reason: {msg}");
        assert_ne!(a.config, dbc_state::default_config_path(), "must not fall back");
    }

    #[test]
    fn a_broken_pointer_is_survivable_by_overriding_exactly_what_is_needed() {
        // serve needs both …
        let a = parse_args_from(&raw(&["--config", "C:\\x.toml", "--vault", "C:\\x.bin"]), broken());
        assert!(matches!(a.command, Command::Serve));
        // … and only one is not enough.
        let a = parse_args_from(&raw(&["--config", "C:\\x.toml"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
        // setup needs only the vault …
        let a = parse_args_from(&raw(&["setup", "--vault", "C:\\x.bin"]), broken());
        assert!(matches!(a.command, Command::Setup { remove: false }));
        // … and without it, it fails.
        let a = parse_args_from(&raw(&["setup"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
        // help needs neither.
        let a = parse_args_from(&raw(&["--help"]), broken());
        assert!(matches!(a.command, Command::Help));
    }

    #[test]
    fn setup_remove_needs_no_paths_at_all() {
        // Revocation deletes a keyring entry; it never opens the vault.
        let a = parse_args_from(&raw(&["setup", "--remove"]), broken());
        assert!(matches!(a.command, Command::Setup { remove: true }));
    }

    #[test]
    fn an_unrecognized_argument_still_prints_usage() {
        let a = parse_args_from(&raw(&["--nonsense"]), profile());
        assert!(matches!(a.command, Command::Help));
    }

    #[test]
    fn the_broken_message_names_the_pointer_and_the_fix_without_leaking_anything() {
        let m = workspace_broken_message(&Some(PathBuf::from("D:\\ws-gone")), "chybí dbc-workspace.toml");
        assert!(m.contains("D:\\ws-gone"));
        assert!(m.contains("chybí dbc-workspace.toml"));
        assert!(m.contains("--config"), "tells the operator the override exists");
        let m = workspace_broken_message(&None, "ukazatel je poškozený");
        assert!(m.contains("ukazatel je poškozený"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-mcp parse_args`
Expected: FAIL — `cannot find function parse_args_from`, `no variant Fail`.

- [ ] **Step 3: Implement**

In `crates/dbc-mcp/src/main.rs`, extend `Command` and split `parse_args`:

```rust
enum Command {
    Serve,
    Setup { remove: bool },
    Help,
    /// Design §W7: the pointer file names a workspace this build cannot
    /// use, AND the command actually needs a path it would have supplied.
    /// One stderr line, non-zero exit — dbc-mcp must not silently serve
    /// profile-mode connections any more than the GUI may silently show
    /// them (§W4).
    Fail(String),
}

/// The stderr message for a broken pointer. Names the folder (or says the
/// pointer itself is unreadable), names the reason, and points at the
/// escape hatch — nothing else: no config contents, no vault bytes, no
/// connection names (`stdout is sacred`, and stderr is a log too).
fn workspace_broken_message(root: &Option<PathBuf>, reason: &str) -> String {
    let where_ = match root {
        Some(r) => r.display().to_string(),
        None => "ukazatel na pracovní prostor je nečitelný".to_string(),
    };
    format!(
        "dbc-mcp: pracovní prostor není použitelný: {where_} ({reason})\n\
         Otevřete aplikaci dbc a prostor obnovte, nebo spusťte dbc-mcp s explicitními cestami: --config <path> --vault <path>"
    )
}

/// Pure core of `parse_args` — takes the raw arguments and the ALREADY
/// resolved workspace state, so the whole precedence rule (explicit flags >
/// workspace defaults > profile defaults; broken ⇒ fail when the default
/// would be used) is unit-testable without env vars or a filesystem.
fn parse_args_from(raw: &[String], res: dbc_state::workspace::Resolution) -> Args {
    use dbc_state::workspace::Resolution;
    let (mut config, mut vault, broken) = match res {
        Resolution::Profile(p) => (p.config, p.vault, None),
        Resolution::Workspace { paths, .. } => (paths.config, paths.vault, None),
        // Deliberately NOT profile paths: if a `Fail` ever leaked through,
        // the paths it carries must not open the profile's real files.
        Resolution::Broken { root, reason } => (
            PathBuf::new(),
            PathBuf::new(),
            Some(workspace_broken_message(&root, &reason)),
        ),
    };
    let mut config_explicit = false;
    let mut vault_explicit = false;
    let mut is_setup = false;
    let mut remove = false;
    let mut help = false;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "setup" => is_setup = true,
            "--remove" => remove = true,
            "--help" | "-h" => help = true,
            "--config" => {
                i += 1;
                if let Some(v) = raw.get(i) {
                    config = v.into();
                    config_explicit = true;
                } else {
                    eprintln!("dbc-mcp: --config requires a path");
                    help = true;
                }
            }
            "--vault" => {
                i += 1;
                if let Some(v) = raw.get(i) {
                    vault = v.into();
                    vault_explicit = true;
                } else {
                    eprintln!("dbc-mcp: --vault requires a path");
                    help = true;
                }
            }
            other => {
                eprintln!("dbc-mcp: unrecognized argument '{other}'");
                help = true;
            }
        }
        i += 1;
    }

    let command = if help {
        Command::Help
    } else if is_setup {
        Command::Setup { remove }
    } else {
        Command::Serve
    };
    // §W7: fatal only when a default the broken pointer would have supplied
    // is actually needed. `--help` needs nothing; `setup --remove` only
    // touches the credential store; `setup` needs the vault; `serve` needs
    // both. Overriding exactly what you need keeps working.
    let needs_config = matches!(command, Command::Serve) && !config_explicit;
    let needs_vault =
        matches!(command, Command::Serve | Command::Setup { remove: false }) && !vault_explicit;
    let command = match broken {
        Some(msg) if needs_config || needs_vault => Command::Fail(msg),
        _ => command,
    };
    Args { config, vault, command }
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // Design §W0.1's SECOND path-resolution call site — the same
    // `dbc_state::workspace::resolve()` the GUI uses. There is exactly one
    // resolution rule in this repo; a second one here would be the
    // divergence §W2 exists to prevent.
    parse_args_from(&raw, dbc_state::workspace::resolve())
}
```

`main()` gains the arm:

```rust
        Command::Fail(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
```

and `print_usage` gains one line under USAGE (the operator has to be able to discover this):

```text
    Cesty se ve výchozím stavu řídí pracovním prostorem nastaveným v aplikaci dbc
    (ukazatel %APPDATA%\dbc\workspace.toml). --config/--vault mají vždy přednost.
```

**Escaping trap:** `print_usage` is one big `eprintln!` format string, so each backslash above must be written as a doubled backslash in the source — a lone `\d` / `\w` is an invalid Rust escape and will NOT compile — and any literal brace needs `{{` / `}}`. The existing JSON example inside that same string already demonstrates both.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-mcp`
Expected: PASS (8 new tests). `PathBuf` must be in scope in the test module — the file already has `use std::path::PathBuf;`.

- [ ] **Step 5: Verify stdout stays sacred**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-mcp -- --help 1>NUL` with a pointer at a non-existent folder; and again without `--help`.
Expected: the `--help` run exits 0; the bare run prints the Czech broken-workspace message **on stderr only** and exits non-zero. `1>NUL` must swallow nothing of it — a single byte of this on stdout corrupts the JSON-RPC stream (crate doc comment).

- [ ] **Step 6: Gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings. Note the change of role recorded in the Global Constraints: `dbc-mcp` is no longer a NONE-diff canary — this task is its real content.

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-mcp/src/main.rs
git commit -m "feat: dbc-mcp resolves the workspace pointer, fails loudly when broken (workspace T6)"
```

---

### Task 7: The scripts flip — „Skripty" live over `effective_scripts_root()`, both arms

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` (`effective_scripts_root`, `start_scripts_scan`, `on_tree_event`'s new arms, `apply_context`'s one added line, `refresh_tree_context`)
- Modify: `crates/dbc-ui/src/schema_tree.rs` (`TreeEvent` variants, the row icons, the chevron/click emissions, the `flatten_sidebar` call site, deletion of every Task 3 `#[allow(dead_code)]`)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`render_settings_panel`'s „Složka skriptů" block / workspace read-only line)

Batch 4, step 1 of 3 — strictly sequential with Tasks 8 and 9, all three own `main.rs`.

**Serialization note (recorded deviation from the batch table):** this task also touches `schema_tree.rs`, which the table lists only under batch 1. That is safe and deliberate: batch 4 is a single sequential lane, so no other task holds `schema_tree.rs` at the same time, and Part S §4 requires the `TreeEvent` variants and their `main.rs` handlers to land in ONE task (the match is exhaustive).

**THE SEAM (design §W8, and the ordering rationale at the top of this plan):** `effective_scripts_root` lands here COMPLETE — both arms, no stub, written once. Workspace mode ⇒ `<workspace>/scripts`; profile mode ⇒ `AppConfig.scripts_dir`. There is no third source and no precedence question: `scripts_dir` is INERT in workspace mode, and the app never writes it while a workspace is active.

**Interfaces:**
- Consumes: `AppView.workspace_root` and `AppView::apply_context` (Task 4); `dbc_state::workspace::SCRIPTS_SUBDIR` (Task 2); `crate::scripts::{scan_scripts, ScriptScan}` (Task 0); `SchemaTree::{begin_scripts_scan, finish_scripts_scan, set_scripts_configured, reset_scripts, scripts_needs_scan}` and `flatten_sidebar`'s `scripts` parameter (Task 3, exact signatures).
- Produces (Tasks 8 and 9 consume these):
  - `pub(crate) fn AppView::effective_scripts_root(&self) -> Option<PathBuf>`
  - `pub(crate) fn AppView::start_scripts_scan(&mut self, cx: &mut Context<Self>)`
  - `TreeEvent::{ScriptsRefresh, OpenScriptsSettings, ScriptOpen { rel: String }, ScriptRunFile { rel: String }, ScriptCreate { parent_rel: String }, ScriptRename { rel: String, is_dir: bool }, ScriptDelete { rel: String, is_dir: bool }}`

- [ ] **Step 1: Write the failing tests**

Add to `crates/dbc-ui/src/main.rs`'s `workspace_startup_tests` module (pure — `effective_scripts_root`'s logic is extracted into a free function so it needs no `AppView`):

```rust
    #[test]
    fn workspace_mode_roots_the_scripts_tree_in_the_folder() {
        let root = PathBuf::from("D:\\ws");
        assert_eq!(
            scripts_root_for(Some(&root), Some("C:\\jinde")),
            Some(root.join("scripts")),
        );
    }

    #[test]
    fn scripts_dir_is_inert_in_workspace_mode() {
        // §W8: one root per mode, no precedence question — a hand-edited
        // `scripts_dir` in a workspace config.toml is ignored, and this is
        // the test that says so out loud.
        let root = PathBuf::from("D:\\ws");
        assert_eq!(scripts_root_for(Some(&root), None), Some(root.join("scripts")));
        assert_eq!(
            scripts_root_for(Some(&root), Some("C:\\jinde")),
            scripts_root_for(Some(&root), None),
        );
    }

    #[test]
    fn profile_mode_uses_the_configured_scripts_dir_or_nothing() {
        assert_eq!(scripts_root_for(None, Some("C:\\skripty")), Some(PathBuf::from("C:\\skripty")));
        assert_eq!(scripts_root_for(None, None), None);
    }

    #[test]
    fn the_scripts_subdir_name_comes_from_dbc_state_not_a_local_literal() {
        assert_eq!(dbc_state::workspace::SCRIPTS_SUBDIR, "scripts");
        let root = PathBuf::from("D:\\ws");
        assert_eq!(
            scripts_root_for(Some(&root), None).unwrap(),
            root.join(dbc_state::workspace::SCRIPTS_SUBDIR),
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui scripts_root_for`
Expected: FAIL — `cannot find function scripts_root_for`.

- [ ] **Step 3: Implement the seam**

In `main.rs`, next to `blocked_paths`:

```rust
/// THE scripts-root seam (design §W8), as a free fn so both arms are
/// testable without an `AppView`. Workspace mode always wins and always
/// resolves to `<workspace>/scripts`: a per-workspace override would
/// reintroduce absolute paths into a folder whose whole point is
/// portability, so `AppConfig.scripts_dir` is INERT there — deliberately
/// not "merged", not "preferred if set". Profile mode is Part S §2's
/// behavior, unchanged.
pub(crate) fn scripts_root_for(workspace_root: Option<&Path>, scripts_dir: Option<&str>) -> Option<PathBuf> {
    match workspace_root {
        Some(root) => Some(root.join(dbc_state::workspace::SCRIPTS_SUBDIR)),
        None => scripts_dir.map(PathBuf::from),
    }
}
```

and on `AppView`:

```rust
    /// The scripts library's root for the ACTIVE context — see
    /// `scripts_root_for`. Every scan and every fs op in Tasks 8/9 starts
    /// here; there is no second resolver.
    pub(crate) fn effective_scripts_root(&self) -> Option<PathBuf> {
        scripts_root_for(self.workspace_root.as_deref(), self.config.scripts_dir.as_deref())
    }

    /// Dispatches a bounded background scan into the tree's scripts slot.
    /// A missing root is NOT an error here — the section renders its
    /// „složka skriptů není nastavena" pointer row instead (Part S §1.4),
    /// and in workspace mode `configured` is always true, so a deleted
    /// `<workspace>/scripts` surfaces honestly as the scan's own error row
    /// plus its retry click.
    pub(crate) fn start_scripts_scan(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.effective_scripts_root() else {
            self.tree.update(cx, |t, cx| {
                t.set_scripts_configured(false, cx);
                t.reset_scripts(cx);
            });
            return;
        };
        self.tree.update(cx, |t, cx| t.set_scripts_configured(true, cx));
        let generation = self.tree.update(cx, |t, cx| t.begin_scripts_scan(cx));
        let tree = self.tree.clone();
        cx.spawn(async move |_this, cx| {
            let result = cx.background_spawn(async move { crate::scripts::scan_scripts(&root) }).await;
            let _ = tree.update(cx, |t, cx| t.finish_scripts_scan(generation, result, cx));
        })
        .detach();
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui scripts_root_for`
Expected: PASS (4 tests).

- [ ] **Step 5: The `TreeEvent` variants and their emissions**

In `schema_tree.rs`, add to `TreeEvent` (Part S §4):

```rust
    /// Scripts library: the „Skripty" root's `⟳` icon, or a retry click on
    /// the section's error row — `main.rs::start_scripts_scan`.
    ScriptsRefresh,
    /// The unconfigured-notice row was clicked — opens „Nastavení"
    /// (discoverability without a wizard, Part S §1.4). Can only ever be
    /// emitted in PROFILE mode: workspace mode is always configured.
    OpenScriptsSettings,
    /// Double-click on a `ScriptFile` row — load the file into the global
    /// editor and bind it (Part S §5.1, Task 8). Opening NEVER runs
    /// anything.
    ScriptOpen { rel: String },
    /// The `▶` icon on a `ScriptFile` row — the unchanged G12 confirm flow
    /// over the file on DISK (Part S §6, Task 9).
    ScriptRunFile { rel: String },
    /// The `+` icon on the root or a folder row — the create dialog
    /// (Task 9). `parent_rel` is `""` for the root itself.
    ScriptCreate { parent_rel: String },
    /// The `✎` icon — the rename dialog (Task 9).
    ScriptRename { rel: String, is_dir: bool },
    /// The `✕` icon — the delete confirm (Task 9). Folders only when empty
    /// (Part S §7.9: no recursive delete in v1).
    ScriptDelete { rel: String, is_dir: bool },
```

Widen `handle_chevron`'s scripts arm (replacing Task 3's toggle-only version):

```rust
            SidebarRow::ScriptsRoot => {
                let was_expanded = self.outer_expanded.contains(&OuterId::Scripts);
                self.toggle_outer(row);
                // Lazy, exactly like a Connection row: expanding a
                // NotLoaded/Error section is what dispatches the scan.
                if !was_expanded && self.scripts_needs_scan() {
                    cx.emit(TreeEvent::ScriptsRefresh);
                }
            }
            SidebarRow::ScriptFolder { .. } => self.toggle_outer(row),
            SidebarRow::ScriptFile { .. } | SidebarRow::ScriptNotice { .. } => {}
```

`handle_single_click`'s notice arm (replacing Task 3's inert one):

```rust
            SidebarRow::ScriptNotice { text, open_settings } => {
                if *open_settings {
                    cx.emit(TreeEvent::OpenScriptsSettings);
                } else if text.starts_with("error:") {
                    cx.emit(TreeEvent::ScriptsRefresh); // retry, the Notice idiom
                }
            }
```

`handle_double_click` — a script file opens into the editor:

```rust
            SidebarRow::ScriptFile { rel } => cx.emit(TreeEvent::ScriptOpen { rel: rel.clone() }),
```

Row icons in `SchemaTree::render`'s `uniform_list` processor, following the ★/⊞/⇪ precedent exactly (always rendered, `cx.stop_propagation()`, then emit — there are no context menus and no tooltips at the pinned rev):

```rust
                            // Scripts library row actions (Part S §4). Not
                            // gated on `in_scope`: the section is GLOBAL —
                            // scripts are files, not database objects.
                            let script_icons: Vec<AnyElement> = match &row_id {
                                SidebarRow::ScriptsRoot => vec![
                                    script_icon(ix, "scripts-refresh", "⟳", cx.theme().text_primary, cx, |cx| {
                                        cx.emit(TreeEvent::ScriptsRefresh)
                                    }),
                                    script_icon(ix, "scripts-new", "+", cx.theme().accent, cx, |cx| {
                                        cx.emit(TreeEvent::ScriptCreate { parent_rel: String::new() })
                                    }),
                                ],
                                SidebarRow::ScriptFolder { rel } => {
                                    let (a, b, c) = (rel.clone(), rel.clone(), rel.clone());
                                    vec![
                                        script_icon(ix, "scripts-new", "+", cx.theme().accent, cx, move |cx| {
                                            cx.emit(TreeEvent::ScriptCreate { parent_rel: a.clone() })
                                        }),
                                        script_icon(ix, "scripts-rename", "✎", cx.theme().text_muted, cx, move |cx| {
                                            cx.emit(TreeEvent::ScriptRename { rel: b.clone(), is_dir: true })
                                        }),
                                        script_icon(ix, "scripts-delete", "✕", cx.theme().danger, cx, move |cx| {
                                            cx.emit(TreeEvent::ScriptDelete { rel: c.clone(), is_dir: true })
                                        }),
                                    ]
                                }
                                SidebarRow::ScriptFile { rel } => {
                                    let (a, b, c) = (rel.clone(), rel.clone(), rel.clone());
                                    vec![
                                        script_icon(ix, "scripts-run", "▶", cx.theme().success, cx, move |cx| {
                                            cx.emit(TreeEvent::ScriptRunFile { rel: a.clone() })
                                        }),
                                        script_icon(ix, "scripts-rename", "✎", cx.theme().text_muted, cx, move |cx| {
                                            cx.emit(TreeEvent::ScriptRename { rel: b.clone(), is_dir: false })
                                        }),
                                        script_icon(ix, "scripts-delete", "✕", cx.theme().danger, cx, move |cx| {
                                            cx.emit(TreeEvent::ScriptDelete { rel: c.clone(), is_dir: false })
                                        }),
                                    ]
                                }
                                _ => Vec::new(),
                            };
```

with the one shared builder (do not hand-roll a second one per icon). It needs two names added to `schema_tree.rs`'s existing `use gpui::{…}` list — `AnyElement` and `Hsla`; `IntoElement` (for `.into_any_element()`) already arrives via `prelude::*`:

```rust
/// One inline scripts row-action icon — the ★/⊞/⇪ shape: always rendered,
/// stops propagation so the row's own click never also fires, then emits.
fn script_icon(
    ix: usize,
    id: &'static str,
    glyph: &'static str,
    color: Hsla,
    cx: &mut Context<SchemaTree>,
    emit: impl Fn(&mut Context<SchemaTree>) + 'static,
) -> AnyElement {
    div()
        .id((id, ix))
        .px_1()
        .flex_shrink_0()
        .cursor_pointer()
        .text_color(color)
        .child(glyph)
        .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            emit(cx);
        }))
        .into_any_element()
}
```

Append `script_icons` into the row's children next to `star`/`diagram_icon`/`csv_icon`.

Finally, the flip itself: `SchemaTree::render`'s `flatten_sidebar(…)` last argument becomes

```rust
            Some((&self.scripts, self.scripts_configured)),
```

and **every `#[allow(dead_code)]` Task 3 added in this file is deleted** (three fields + one impl block). Grep to be sure: `grep -n "DARK UNTIL TASK 7" crates/dbc-ui/src/schema_tree.rs` must return nothing when this step is done.

- [ ] **Step 6: `main.rs` handlers**

In `on_tree_event`:

```rust
            TreeEvent::ScriptsRefresh => self.start_scripts_scan(cx),
            TreeEvent::OpenScriptsSettings => self.open_settings(cx),
            // Tasks 8 and 9 fill these three in — they land here now only
            // because `TreeEvent`'s match is exhaustive and its variants
            // must be emitted and handled in one task. Each is an HONEST
            // "not yet" (a Czech status, no silent swallow), and each is
            // replaced — not extended — by its owning task's step.
            TreeEvent::ScriptOpen { .. }
            | TreeEvent::ScriptRunFile { .. }
            | TreeEvent::ScriptCreate { .. }
            | TreeEvent::ScriptRename { .. }
            | TreeEvent::ScriptDelete { .. } => {
                self.status = "error: tato akce zatím není dostupná".to_string();
                cx.notify();
            }
```

**Task 8 replaces the `ScriptOpen` arm; Task 9 replaces the remaining four and deletes this grouped arm entirely.** Task 10's placeholder sweep greps for the string „tato akce zatím není dostupná" — it must not survive the phase.

Then: `refresh_tree_context` pushes the configured flag alongside the other tree-context pushes:

```rust
        let configured = self.effective_scripts_root().is_some();
        self.tree.update(cx, |t, cx| t.set_scripts_configured(configured, cx));
```

`apply_context` gains its ONE promised line (Task 4 left the comment there — replace the comment with the code):

```rust
        // §W3.4: the scripts root just changed under us. Clear to
        // `NotLoaded` (dropping stale expand keys and any in-flight scan of
        // the OLD root), then rescan the new one.
        self.tree.update(cx, |t, cx| t.reset_scripts(cx));
        self.start_scripts_scan(cx);
```

and `fn main()`'s post-window wiring gains `view.start_scripts_scan(cx);` right after `view.refresh_tree_context(cx);` — Part S §1.2's "automatic scan on startup (when configured)"; `start_scripts_scan` is a no-op-with-reset when there is no root, and the blocked start has none.

- [ ] **Step 7: Settings — „Složka skriptů" (profile) / the fixed line (workspace)**

In `render_settings_panel`, under the „Pracovní prostor" block from Task 5:

```rust
        panel = panel.child(div().text_color(cx.theme().text_muted).child("Složka skriptů"));
        match &ws_root {
            // §W8: in workspace mode the block is a fixed read-only line —
            // no picker, no „Odebrat". The root is a convention, not a
            // setting.
            Some(root) => {
                panel = panel.child(div().child(format!(
                    "Skripty: {}",
                    root.join(dbc_state::workspace::SCRIPTS_SUBDIR).display()
                )));
            }
            None => {
                let current = self
                    .config
                    .scripts_dir
                    .clone()
                    .unwrap_or_else(|| "nenastavena".to_string());
                panel = panel
                    .child(div().text_color(cx.theme().text_muted).child(current))
                    .child(
                        div()
                            .id("settings-scripts-pick")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Vybrat složku…")
                            .on_click(cx.listener(|this, _, _, cx| this.start_scripts_dir_pick(cx))),
                    )
                    .child(
                        div()
                            .id("settings-scripts-clear")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Odebrat")
                            .on_click(cx.listener(|this, _, _, cx| this.clear_scripts_dir(cx))),
                    );
            }
        }
```

with, in `main.rs`:

```rust
    /// Part S §2 — PROFILE mode only (the caller only renders the button
    /// there). Stores the absolute path, saves the config, rescans.
    fn start_scripts_dir_pick(&mut self, cx: &mut Context<Self>) {
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Vybrat".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = dialog.await else { return };
            if paths.is_empty() {
                return;
            }
            let picked = paths.remove(0);
            let _ = this.update(cx, |view, cx| {
                // Defense in depth: `scripts_dir` is inert in workspace
                // mode, so the app must never WRITE it there either (§W8).
                if view.workspace_root.is_some() {
                    return;
                }
                view.config.scripts_dir = Some(picked.display().to_string());
                view.status = match view.config.save(&view.config_path) {
                    Ok(()) => format!("složka skriptů: {}", picked.display()),
                    Err(e) => format!("error: nastavení se nepodařilo uložit ({e})"),
                };
                view.start_scripts_scan(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Part S §2's „Odebrat": clears the setting and the tree state. It
    /// deliberately does NOT touch a script binding — the binding holds an
    /// ABSOLUTE path, so „Uložit" in the caption strip keeps working; the
    /// resolved §2 note says so explicitly, and there is no guard here.
    fn clear_scripts_dir(&mut self, cx: &mut Context<Self>) {
        if self.workspace_root.is_some() {
            return;
        }
        self.config.scripts_dir = None;
        self.status = match self.config.save(&self.config_path) {
            Ok(()) => "složka skriptů odebrána".to_string(),
            Err(e) => format!("error: nastavení se nepodařilo uložit ({e})"),
        };
        self.start_scripts_scan(cx); // no root ⇒ resets the tree slot
        cx.notify();
    }
```

- [ ] **Step 8: Manual verification of BOTH arms**

`%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`:
1. Profile mode, no `scripts_dir`: „Skripty" expands to „složka skriptů není nastavena — klikněte pro Nastavení"; the click opens „Nastavení".
2. Pick a folder holding `a.sql` and `sub\b.sql`: the tree shows `sub` and `a.sql`; expanding `sub` shows `b.sql`; `⟳` re-scans after an external `copy` into the folder.
3. Switch to a workspace (Task 5's flow): the Settings block turns into the fixed „Skripty: {workspace}\scripts" line with no picker, and the tree re-roots to the (empty) workspace `scripts\` — „žádné skripty (*.sql)".
4. Hand-edit `scripts_dir` into the WORKSPACE `config.toml`, restart: the tree still shows `<workspace>\scripts` (§W8 inertness, verified once by hand).

- [ ] **Step 9: Gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings — and `grep -rn "DARK UNTIL TASK 7" crates/` returns nothing.

- [ ] **Step 10: Commit**

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: scripts section live over effective_scripts_root (both arms), settings blocks (workspace T7)"
```

---

### Task 8: Editor binding — `ScriptBinding`, caption strip, Ctrl+S, discard guard

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` (`actions!`, `bind_keys`, `ScriptBinding`, `PendingScriptAction`, `PendingDiscard::Script`, `AppView.script_binding`, `open_script`, `save_script`, `save_script_as`, `unbind_script`, `editor_load_guarded`, `render_script_caption`, `on_discard_confirm_yes`, `context_switch_blocked`)
- Modify: `crates/dbc-ui/src/history_panel.rs` (route its row click through the guard)
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::SaveScript` + its label)

Batch 4, step 2 of 3 — strictly after Task 7, strictly before Task 9.

**Grounding (Part S fact 0.1, verified):** tabs are RESULT tabs only; the SQL editor is ONE global `Entity<SqlInput>` (`AppView.sql`). There is no per-tab editor state and no `Ctrl+S`/`Ctrl+O` binding anywhere in the repo. So "opening a script" binds the single global editor — building per-script editor tabs would be an editor-architecture rework (the g6-editor-pro draft's territory), not this phase.

**Interfaces:**
- Consumes: `AppView::effective_scripts_root()` (Task 7); `crate::scripts::{resolve_rel, read_script, write_script, validate_script_name, SCRIPT_OPEN_CAP}` (Task 0); `AppView::context_switch_blocked` (Task 5) — **extended here, not duplicated**; the existing `DiscardConfirmState { change_count: usize, action: PendingDiscard }` machinery.
- Produces (Task 9 consumes these):
  - `pub(crate) struct ScriptBinding { pub path: PathBuf, pub saved_text: String }`
  - `AppView.script_binding: Option<ScriptBinding>`
  - `pub(crate) fn AppView::script_is_dirty(&self, cx: &App) -> bool`
  - `AppView.script_dirty_flag: bool` (recomputed once per frame in `AppView::render`; the only dirtiness source readable without a `cx`)
  - `pub(crate) fn AppView::binding_rel(&self) -> Option<String>`
  - `pub(crate) enum PendingScriptAction { Open { rel: String }, Unbind, LoadText { sql: String } }` and `PendingDiscard::Script(PendingScriptAction)`
  - `pub(crate) fn AppView::editor_load_guarded(&mut self, action: PendingScriptAction, cx: &mut Context<Self>)`
  - `pub(crate) fn AppView::bind_script(&mut self, path: PathBuf, text: String, cx: &mut Context<Self>)`

- [ ] **Step 1: Write the failing tests**

Add a module to `crates/dbc-ui/src/main.rs` (pure — the dirty rule and the caption are extracted into free fns so no GPUI entity is needed):

```rust
#[cfg(test)]
mod script_binding_tests {
    use super::*;

    #[test]
    fn dirty_is_an_exact_compare_with_a_length_short_circuit() {
        assert!(!script_text_is_dirty("SELECT 1", "SELECT 1"));
        assert!(script_text_is_dirty("SELECT 1", "SELECT 2"));
        assert!(script_text_is_dirty("SELECT 1", "SELECT 1 "), "trailing space counts");
        // Whitespace-only differences are REAL differences: the file is the
        // truth and „ •" must not lie about it.
        assert!(script_text_is_dirty("a\r\nb", "a\nb"), "line endings count");
    }

    #[test]
    fn the_caption_relativizes_against_the_current_root_and_falls_back_to_the_name() {
        let root = PathBuf::from("D:\\ws\\scripts");
        assert_eq!(
            script_caption_rel(&root.join("prod").join("trzby.sql"), Some(&root)),
            "prod/trzby.sql"
        );
        // Outside the root (save-as onto the desktop, or the root changed
        // under a binding that holds an ABSOLUTE path) ⇒ bare file name.
        assert_eq!(
            script_caption_rel(Path::new("C:\\jinde\\ad-hoc.sql"), Some(&root)),
            "ad-hoc.sql"
        );
        assert_eq!(script_caption_rel(Path::new("C:\\jinde\\ad-hoc.sql"), None), "ad-hoc.sql");
    }

    #[test]
    fn the_caption_uses_the_tab_title_dirty_convention_exactly() {
        assert_eq!(script_caption("prod/trzby.sql", false), "Skript: prod/trzby.sql");
        assert_eq!(script_caption("prod/trzby.sql", true), "Skript: prod/trzby.sql •");
    }

    #[test]
    fn the_open_cap_refusal_names_the_limit_and_the_way_out() {
        assert_eq!(
            script_open_refusal(crate::scripts::SCRIPT_OPEN_CAP + 1),
            Some("soubor je příliš velký pro editor (limit 1 MiB) — spusťte jej jako skript".to_string())
        );
        assert_eq!(script_open_refusal(crate::scripts::SCRIPT_OPEN_CAP), None);
    }

    #[test]
    fn save_as_appends_sql_when_missing_and_never_twice() {
        // Fact 0.6: GPUI file dialogs have no extension filter at the
        // pinned rev, so the `.sql` rule is client-side, here.
        assert_eq!(with_sql_extension(Path::new("C:\\a\\dotaz")), PathBuf::from("C:\\a\\dotaz.sql"));
        assert_eq!(with_sql_extension(Path::new("C:\\a\\dotaz.sql")), PathBuf::from("C:\\a\\dotaz.sql"));
        assert_eq!(with_sql_extension(Path::new("C:\\a\\dotaz.SQL")), PathBuf::from("C:\\a\\dotaz.SQL"));
        assert_eq!(with_sql_extension(Path::new("C:\\a\\dotaz.txt")), PathBuf::from("C:\\a\\dotaz.txt.sql"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui script_binding`
Expected: FAIL — `cannot find function script_text_is_dirty` etc.

- [ ] **Step 3: The pure helpers**

In `main.rs`:

```rust
/// Part S §5: dirty = the editor text differs from what was last read from
/// (or written to) disk. Exact compare, bounded by the 1 MiB open cap;
/// `String`'s `!=` already short-circuits on length.
fn script_text_is_dirty(editor: &str, saved: &str) -> bool {
    editor != saved
}

/// The binding's display path: relative to the CURRENT scripts root when
/// it lives under it, otherwise the bare file name. The binding itself
/// holds an ABSOLUTE path (resolved rejected alternative: storing a rel
/// breaks the moment the root changes) — this is only the label.
fn script_caption_rel(path: &Path, root: Option<&Path>) -> String {
    if let Some(root) = root {
        if let Ok(rel) = path.strip_prefix(root) {
            return rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
        }
    }
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// The caption strip's label — the EXACT tab-title dirty convention (`" •"`).
fn script_caption(rel: &str, dirty: bool) -> String {
    if dirty {
        format!("Skript: {rel} •")
    } else {
        format!("Skript: {rel}")
    }
}

/// Part S §7.6: the editor-open cap. Running via ▶ has NO such cap (it
/// streams through the G12 splitter in 64 KiB chunks) — hence the pointer
/// to that route in the refusal.
fn script_open_refusal(size: u64) -> Option<String> {
    (size > crate::scripts::SCRIPT_OPEN_CAP).then(|| {
        "soubor je příliš velký pro editor (limit 1 MiB) — spusťte jej jako skript".to_string()
    })
}

/// Part S §5.4 / fact 0.6: `.sql` is enforced client-side because the
/// pinned GPUI rev's `prompt_for_new_path` has no extension filter.
fn with_sql_extension(path: &Path) -> PathBuf {
    let is_sql =
        path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("sql"));
    if is_sql {
        path.to_path_buf()
    } else {
        let mut s = path.as_os_str().to_owned();
        s.push(".sql");
        PathBuf::from(s)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui script_binding`
Expected: PASS (5 tests).

- [ ] **Step 5: State, action, key binding**

```rust
/// Part S §1.3: the app has no editor TABS (fact 0.1) — opening a script
/// binds the ONE global editor to a file. `path` is ABSOLUTE so the binding
/// survives a scripts-root change; the caption re-relativizes for display.
/// `saved_text` is what is on disk as far as this session knows — the
/// dirty flag is `sql.text() != saved_text`.
pub(crate) struct ScriptBinding {
    pub path: PathBuf,
    pub saved_text: String,
}
```

`AppView` gains `script_binding: Option<ScriptBinding>` (initialized `None` in `main()`), plus:

```rust
    pub(crate) fn script_is_dirty(&self, cx: &App) -> bool {
        self.script_binding
            .as_ref()
            .is_some_and(|b| script_text_is_dirty(&self.sql.read(cx).text(), &b.saved_text))
    }

    pub(crate) fn binding_rel(&self) -> Option<String> {
        let b = self.script_binding.as_ref()?;
        Some(script_caption_rel(&b.path, self.effective_scripts_root().as_deref()))
    }

    /// Sets the editor text AND the binding in one place, so the two can
    /// never drift (a `set_text` without a matching `saved_text` update is
    /// exactly how a phantom „ •" appears).
    pub(crate) fn bind_script(&mut self, path: PathBuf, text: String, cx: &mut Context<Self>) {
        self.sql.update(cx, |s, cx| s.set_text(&text, cx));
        self.script_binding = Some(ScriptBinding { path, saved_text: text });
        self.status = String::new();
        cx.notify();
    }
```

Action + chord (the `ctrl-s` chord is free — verified repo-wide):

```rust
actions!(
    dbc,
    [RunQuery, RunQueryUnlimited, CancelQuery, ToggleTree, ToggleHistory, OpenPalette, OpenAutocomplete, SaveScript]
);
```

```rust
            // Part S §5.2/§5.4: global, context `None` — the same posture
            // as `RunQuery`/`OpenPalette`. Bound ⇒ save; unbound ⇒ save-as.
            KeyBinding::new("ctrl-s", SaveScript, None),
```

and the handler registered on `AppView`'s root element alongside the existing `on_action` listeners:

```rust
    fn on_save_script(&mut self, _: &SaveScript, _window: &mut Window, cx: &mut Context<Self>) {
        match &self.script_binding {
            Some(b) => {
                let (path, text) = (b.path.clone(), self.sql.read(cx).text());
                self.save_script(path, text, cx);
            }
            None => self.save_script_as(cx),
        }
    }
```

- [ ] **Step 6: Open / save / save-as / unbind, and the ONE guard**

```rust
/// Part S §5.5: what a dirty binding is parked on. `LoadText` covers the
/// two pre-existing "load SQL into the editor" sites (the history panel row
/// and the palette's history item) — which today clobber the editor with NO
/// guard at all; this phase strictly improves that for BOUND scripts and
/// leaves unbound ad-hoc text exactly as (un)guarded as before.
#[derive(Clone)]
pub(crate) enum PendingScriptAction {
    Open { rel: String },
    Unbind,
    LoadText { sql: String },
}
```

`PendingDiscard` gains `Script(PendingScriptAction)`; `on_discard_confirm_yes` gains its arm:

```rust
            PendingDiscard::Script(action) => self.perform_script_action(action, cx),
```

(`on_discard_confirm_no` needs no arm — dropping the state IS the cancel, exactly like the other variants.)

```rust
    /// THE guard (Part S §5.5). Every site that would replace the editor's
    /// text routes through here; there is no second dirty check.
    pub(crate) fn editor_load_guarded(
        &mut self,
        action: PendingScriptAction,
        cx: &mut Context<Self>,
    ) {
        if self.script_is_dirty(cx) && self.discard_confirm.is_none() {
            self.discard_confirm = Some(DiscardConfirmState {
                // Scripts are text, not staged rows — the count the dialog
                // renders is "one file", and the message branch below is
                // what actually names it.
                change_count: 1,
                action: PendingDiscard::Script(action),
            });
            cx.notify();
            return;
        }
        self.perform_script_action(action, cx);
    }

    fn perform_script_action(&mut self, action: PendingScriptAction, cx: &mut Context<Self>) {
        match action {
            PendingScriptAction::Open { rel } => self.open_script(rel, cx),
            PendingScriptAction::Unbind => {
                // §5.3: the text STAYS — it is simply no longer bound.
                self.script_binding = None;
                self.status = String::new();
                cx.notify();
            }
            PendingScriptAction::LoadText { sql } => {
                self.sql.update(cx, |s, cx| s.set_text(&sql, cx));
                self.script_binding = None;
                cx.notify();
            }
        }
    }

    /// Part S §5.1. Opening NEVER runs anything (the brief's binding rule:
    /// script files are user content).
    fn open_script(&mut self, rel: String, cx: &mut Context<Self>) {
        let Some(root) = self.effective_scripts_root() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let path = match crate::scripts::resolve_rel(&root, &rel) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("error: {e}");
                cx.notify();
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let job = path.clone();
            let result: Result<String, String> = cx
                .background_spawn(async move {
                    let size = std::fs::metadata(&job)
                        .map_err(|e| format!("soubor nelze otevřít: {e}"))?
                        .len();
                    if let Some(refusal) = script_open_refusal(size) {
                        return Err(refusal);
                    }
                    // Non-UTF-8 is an ERROR, never a lossy mangle: a
                    // silently mangled script that is then SAVED would
                    // corrupt the user's file.
                    let bytes = std::fs::read(&job).map_err(|e| format!("soubor nelze otevřít: {e}"))?;
                    String::from_utf8(bytes).map_err(|_| "soubor není platné UTF-8".to_string())
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(text) => view.bind_script(path.clone(), text, cx),
                Err(e) => {
                    view.status = format!("error: {e}");
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Part S §5.2. Atomic (the shared `fsutil::write_atomic` rail, via
    /// `scripts::write_script`). Last-writer-wins on external edits — by
    /// the user's own model git is the history layer; the app does not
    /// diff or version.
    fn save_script(&mut self, path: PathBuf, text: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (job_path, job_text) = (path.clone(), text.clone());
            let result = cx
                .background_spawn(async move { crate::scripts::write_script(&job_path, &job_text) })
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(()) => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        view.script_binding = Some(ScriptBinding { path: path.clone(), saved_text: text.clone() });
                        view.status = format!("skript uložen: {name}");
                    }
                    Err(e) => view.status = format!("error: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Part S §5.4 — Ctrl+S with no binding.
    fn save_script_as(&mut self, cx: &mut Context<Self>) {
        let text = self.sql.read(cx).text();
        if text.trim().is_empty() {
            self.status = "editor je prázdný".to_string();
            cx.notify();
            return;
        }
        let Some(root) = self.effective_scripts_root() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let dialog = cx.prompt_for_new_path(&root, Some("dotaz.sql"));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(picked))) = dialog.await else { return };
            let path = with_sql_extension(&picked);
            let _ = this.update(cx, |view, cx| {
                view.save_script(path.clone(), text.clone(), cx);
                // Rescan when the save landed INSIDE the library; outside is
                // allowed (it is the user's disk) but the tree honestly
                // won't show it.
                if path.starts_with(&root) {
                    view.start_scripts_scan(cx);
                }
            });
        })
        .detach();
    }
```

Route the two pre-existing editor-clobber sites through the guard:
- `main.rs`'s palette `PaletteItem::HistoryEntry` arm: replace `self.sql.update(cx, |s, cx| s.set_text(&sql, cx));` with `self.editor_load_guarded(PendingScriptAction::LoadText { sql }, cx);` (keep the focus call after it).
- `history_panel.rs`'s row click (`view.sql.update(cx, |sql, cx| sql.set_text(&sql_for_click, cx));`) with `view.editor_load_guarded(PendingScriptAction::LoadText { sql: sql_for_click.clone() }, cx);`.

`TreeEvent::ScriptOpen` in `on_tree_event` — **replace** Task 7's placeholder arm:

```rust
            TreeEvent::ScriptOpen { rel } => {
                self.editor_load_guarded(PendingScriptAction::Open { rel: rel.clone() }, cx)
            }
```

Extend `context_switch_blocked` (Task 5's function — do NOT write a second gate):

```rust
        // §W3.1: the Part S §5.5 dirty guard runs first. A context switch
        // re-roots the scripts tree; a dirty buffer must not be stranded.
        if self.script_binding.is_some() && self.script_dirty_flag {
            return Some("skript má neuložené změny — nejprve jej uložte nebo zavřete".to_string());
        }
```

`context_switch_blocked` takes no `cx`, so the dirtiness must be readable without one: add `AppView.script_dirty_flag: bool`, recomputed in `AppView::render` (which already polls `refresh_autocomplete`/`history_search` lazily — the SAME established lazy-poll idiom, see history_panel.rs's module doc comment) via `self.script_dirty_flag = self.script_is_dirty(cx);`. That one line also feeds the caption strip, so it is computed once per frame, not twice.

- [ ] **Step 7: The caption strip**

In `AppView::render`, immediately above the fixed-height editor div (`div().h(px(20. * 8. + 4. * 2.))`), render only when bound:

```rust
        if let Some(rel) = self.binding_rel() {
            let dirty = self.script_dirty_flag;
            column = column.child(
                div()
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .bg(theme.bg_app)
                    .text_color(theme.text_muted)
                    .child(div().flex_1().min_w_0().overflow_hidden().child(script_caption(&rel, dirty)))
                    .child(
                        div()
                            .id("script-save")
                            .px_1()
                            .cursor_pointer()
                            // Dim when clean — the save is a no-op then.
                            .text_color(if dirty { theme.text_primary } else { theme.border })
                            .child("Uložit")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_save_script(&SaveScript, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id("script-unbind")
                            .px_1()
                            .cursor_pointer()
                            .child("Zavřít")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor_load_guarded(PendingScriptAction::Unbind, cx)
                            })),
                    ),
            );
        }
```

Note the `column` binding must be `let mut column = …` before this (it already is).

Discard-dialog copy: the existing `DiscardConfirmState` renderer gains a `PendingDiscard::Script` message branch — „Neuložené změny skriptu {name} budou zahozeny." where `{name}` is `binding_rel()`'s value.

- [ ] **Step 8: Palette entry**

`palette.rs`: `PaletteAction::SaveScript` with the doc comment

```rust
    /// Part S §8: the palette gains exactly ONE scripts item — per-script
    /// palette rows would require the palette to hold the scan, which is a
    /// follow-up candidate, not this phase.
    SaveScript,
```

listed unconditionally in `fixed_actions` next to the two existing script rows:

```rust
        ("Uložit skript".to_string(), PaletteAction::SaveScript),
```

**Careful:** `backup_restore_actions_present_and_last_when_connection_active` asserts the LAST two rows — insert this among the leading unconditional rows (right after „Spustit SQL složku…"), never at the end. Dispatch in `main.rs`: `PaletteAction::SaveScript => self.on_save_script(&SaveScript, window, cx),`.

- [ ] **Step 9: Manual verification**

`%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`, with a configured library:
1. Double-click `a.sql` → the caption reads „Skript: a.sql", the editor holds the file, NOTHING ran.
2. Type a character → the caption becomes „Skript: a.sql •", „Uložit" brightens. Ctrl+S → „skript uložen: a.sql", the „ •" disappears, and no `*.tmp` remains in the folder.
3. With a dirty buffer, double-click `b.sql` → the discard confirm names „Neuložené změny skriptu a.sql budou zahozeny."; „Zrušit" leaves `a.sql` bound and dirty; „Zahodit" opens `b.sql`.
4. „Zavřít" unbinds; the editor text stays. Ctrl+S now opens save-as, defaulting into the library; saving `dotaz` writes `dotaz.sql` and the tree shows it.
5. With a dirty binding, Nastavení → „Přejít na lokální profil" refuses with „error: skript má neuložené změny — nejprve jej uložte nebo zavřete".
6. Open a >1 MiB `.sql` → „error: soubor je příliš velký pro editor (limit 1 MiB) — spusťte jej jako skript".

- [ ] **Step 10: Gate + commit**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings.

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/history_panel.rs crates/dbc-ui/src/palette.rs
git commit -m "feat: script editor binding — caption strip, Ctrl+S save/save-as, dirty guard (workspace T8)"
```

---

### Task 9: Create / rename / delete modals + `▶` run via a factored `open_script_run_modal`

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::ScriptName`, `ModalState::ScriptDeleteConfirm`, `modal_confirm_kind`, their render panels)
- Modify: `crates/dbc-ui/src/main.rs` (`open_script_run_modal` factoring, `start_script_pick` rewired onto it, `run_script_from_library`, the create/rename/delete dispatchers, binding fixups, the Esc allow-list arms)

Batch 4, step 3 of 3.

**THE FACTORING RULE (Part S §6, binding):** the post-pre-scan continuation of `start_script_pick` — the modal-race check, the `conn_identity` re-check, and the `ModalState::ScriptRun` construction — becomes ONE helper that BOTH paths call. The scripts library must not fork the confirm policy. Everything downstream (`confirm_script_run`'s re-checks, `script_run_dispatch_allowed`, the tx/error radios, the runner's per-statement read-only gate, the progress tab, history's `[skript]` synthetic entry) stays untouched by construction. **`▶` always runs the DISK content — never auto-save-before-run** (that would be a silent write); a dirty binding means editor and disk differ, the „ •" makes that visible, and the confirm modal's from-disk statement count is the honest number.

**Interfaces:**
- Consumes: `AppView::{effective_scripts_root, start_scripts_scan}` (Task 7); `AppView.script_binding` and `AppView.script_dirty_flag` (Task 8); `crate::scripts::{resolve_rel, create_script, create_folder, rename_entry, delete_entry}` (Task 0 — **use the names Task 0 Step 4 recorded**; `validate_script_name`, the collision probe and `resolve_parent_rel` are called INSIDE those ops, never again here); the existing `count_statements_in_file`, `conn_identity_matches`, `current_conn_identity`, `resolve_spec_for_explain`, `dialect_for_engine`, `connections_ui::TextField::form_field`.
- Produces:
  - `fn AppView::open_script_run_modal(&mut self, source_label: String, files: Vec<PathBuf>, file_counts: Vec<usize>, conn_label: String, conn_identity: String, read_only: bool, timeout_secs: Option<u64>, cx: &mut Context<Self>)`
  - `connections_ui::ModalState::ScriptName { mode: ScriptNameMode, parent_rel: String, target_rel: String, is_dir: bool, field: Entity<TextField>, error: Option<String> }`
  - `connections_ui::ModalState::ScriptDeleteConfirm { rel: String, is_dir: bool, dirty_bound: bool, error: Option<String> }`
  - `pub(crate) enum connections_ui::ScriptNameMode { NewScript, NewFolder, Rename }`

- [ ] **Step 1: Write the failing tests**

Add to `connections_ui.rs`'s test module (pure):

```rust
    #[test]
    fn script_name_modal_titles_follow_the_designed_copy() {
        assert_eq!(script_name_title(ScriptNameMode::NewScript), "Nový skript");
        assert_eq!(script_name_title(ScriptNameMode::NewFolder), "Nová složka");
        assert_eq!(script_name_title(ScriptNameMode::Rename), "Přejmenovat");
    }

    #[test]
    fn the_delete_confirm_text_names_the_kind_and_the_irreversibility() {
        assert_eq!(
            script_delete_text("trzby.sql", false),
            "Smazat skript trzby.sql? Akce je nevratná (maže se z disku, ne do koše)."
        );
        assert_eq!(
            script_delete_text("prod", true),
            "Smazat složku prod? Akce je nevratná (maže se z disku, ne do koše)."
        );
    }

    #[test]
    fn deleting_a_dirty_bound_file_says_so_in_the_same_modal() {
        // Part S §4's resolved simplification: ONE modal, both facts — no
        // discard-confirm stacked in front of a delete-confirm.
        assert_eq!(
            script_delete_dirty_line(),
            "Skript má neuložené změny v editoru."
        );
    }

    #[test]
    fn a_delete_confirm_never_takes_enter() {
        // §3-novela's substance is IRREVERSIBILITY, not SQL: the button is
        // the last gate before an unrecoverable disk delete.
        assert_eq!(
            modal_confirm_kind(&ModalState::ScriptDeleteConfirm {
                rel: "a.sql".into(),
                is_dir: false,
                dirty_bound: false,
                error: None,
            }),
            ModalConfirmKind::Ignore
        );
    }

    #[test]
    fn the_name_dialog_does_take_enter() {
        // Policy clause (a): confirm creates/renames a FILE and runs
        // NOTHING against the database. `ModalState::ScriptName` holds an
        // `Entity<TextField>`, which cannot be built without a GPUI
        // context — so the policy is pinned through the same free fn
        // `modal_confirm_kind`'s arm calls, giving the table and the test
        // ONE source instead of two.
        assert_eq!(script_name_confirm_kind(), ModalConfirmKind::ScriptName);
    }
```

The shared source, next to `script_name_title` in `connections_ui.rs`:

```rust
/// The Enter policy for `ModalState::ScriptName`, factored out so it can be
/// asserted without constructing the variant's `Entity<TextField>`.
pub(crate) fn script_name_confirm_kind() -> ModalConfirmKind {
    ModalConfirmKind::ScriptName
}
```

and `modal_confirm_kind`'s arm is `ModalState::ScriptName { .. } => script_name_confirm_kind(),`.

Add to `main.rs`'s `script_binding_tests` module (Task 8 created it):

```rust
    #[test]
    fn running_a_library_script_never_auto_saves_first() {
        // Part S §1.3, recorded as a TEST because it is the kind of
        // "helpful" behaviour a future edit adds by accident: ▶ runs what
        // is on DISK. The pre-scan reads the file; nothing writes it.
        // CARGO_MANIFEST_DIR, not `file!()`: cargo runs tests with the
        // PACKAGE dir as CWD while `file!()` is workspace-relative.
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .unwrap();
        let run_fn = src
            .split("fn run_script_from_library")
            .nth(1)
            .expect("run_script_from_library exists");
        let body = &run_fn[..run_fn.find("\n    fn ").unwrap_or(run_fn.len())];
        for banned in ["save_script", "write_script", "set_text"] {
            assert!(!body.contains(banned), "▶ must not {banned}: it runs the DISK content");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui script_name`
Expected: FAIL — `cannot find function script_name_title` / `no variant ScriptDeleteConfirm`.

- [ ] **Step 3: Factor the G12 confirm continuation**

In `main.rs`, lift the tail of `start_script_pick` verbatim into:

```rust
    /// Part S §6 step 3: the SHARED post-pre-scan continuation of the G12
    /// script-run flow. Both the ad-hoc picker (`start_script_pick`) and the
    /// library's `▶` (`run_script_from_library`) end here, so there is
    /// exactly ONE place that decides the modal races and the connection
    /// identity re-check. Moving a single line of this into a caller forks
    /// the confirm policy — that is the defect this factoring prevents.
    #[allow(clippy::too_many_arguments)]
    fn open_script_run_modal(
        &mut self,
        source_label: String,
        files: Vec<PathBuf>,
        file_counts: Vec<usize>,
        conn_label: String,
        conn_identity: String,
        read_only: bool,
        timeout_secs: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        // Review fix (MINOR 4), carried verbatim: a modal the user opened
        // WHILE the pick/pre-scan was in flight wins.
        if self.modal.is_some() {
            self.status = "výběr skriptu zahozen — je otevřený jiný dialog".to_string();
            cx.notify();
            return;
        }
        // Review fix (MAJOR 1), carried verbatim: the pre-scan didn't block
        // the connection dropdown. `confirm_script_run` re-checks this same
        // identity again regardless — this is the faster, friendlier
        // refusal, not the guard.
        if !conn_identity_matches(&conn_identity, &self.current_conn_identity()) {
            self.status = "připojení se během výběru změnilo — spuštění zrušeno".to_string();
            cx.notify();
            return;
        }
        self.status = String::new();
        self.modal = Some(connections_ui::ModalState::ScriptRun {
            files,
            file_counts,
            tx_scope: runner::TxScope::PerFile,
            error_policy: runner::ErrorPolicy::Stop,
            source_label,
            conn_label,
            read_only,
            timeout_secs,
            conn_identity,
        });
        self.modal_needs_focus = true;
        cx.notify();
    }
```

and rewrite `start_script_pick`'s `Ok((source_label, files, file_counts)) => { … }` arm to a single call:

```rust
                Ok((source_label, files, file_counts)) => view.open_script_run_modal(
                    source_label,
                    files,
                    file_counts,
                    conn_label,
                    conn_identity,
                    read_only,
                    timeout_secs,
                    cx,
                ),
```

Run `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` here, BEFORE adding the second caller: every pre-existing G12 test must still be green. A pure refactor with a green suite is the evidence that the continuation was moved, not rewritten.

- [ ] **Step 4: `▶` — the library's run path**

```rust
    /// Part S §6: same entry gates as `start_script_pick`, same
    /// `conn_identity` captured BEFORE the pre-scan, then the SHARED
    /// continuation. Runs the file ON DISK — never the editor buffer, and
    /// never a save first (§1.3: auto-saving before a run would be a silent
    /// write; the „ •" already tells the user the two differ, and the
    /// modal's count is the from-disk truth).
    fn run_script_from_library(&mut self, rel: String, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.cancel.is_some() {
            return;
        }
        let Some(root) = self.effective_scripts_root() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let Some((read_only, timeout_secs, engine, _spec)) = self.resolve_spec_for_explain(cx)
        else {
            return; // resolve_spec_for_explain already set self.status
        };
        let Some(dialect) = dialect_for_engine(engine) else {
            self.status = "error: skripty nejsou podporovány pro tento engine".to_string();
            cx.notify();
            return;
        };
        let conn_label = self.current_connection_label();
        let conn_identity = self.current_conn_identity();
        cx.spawn(async move |this, cx| {
            let result: Result<(String, PathBuf, usize), String> = cx
                .background_spawn(async move {
                    let path = crate::scripts::resolve_rel(&root, &rel)?;
                    let is_sql = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
                    if !is_sql {
                        return Err("vyberte soubor .sql".to_string());
                    }
                    // A stale tree (external delete since the last scan) is
                    // a Czech error plus a rescan, never a corruption.
                    if !path.is_file() {
                        return Err("soubor už neexistuje".to_string());
                    }
                    let count = count_statements_in_file(&path, dialect)?;
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    Ok((name, path, count))
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok((label, path, count)) => view.open_script_run_modal(
                    label,
                    vec![path],
                    vec![count],
                    conn_label,
                    conn_identity,
                    read_only,
                    timeout_secs,
                    cx,
                ),
                Err(e) => {
                    view.status = format!("error: {e}");
                    view.start_scripts_scan(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }
```

- [ ] **Step 5: The name and delete modals**

In `connections_ui.rs`:

```rust
/// Which flavour of the ONE name dialog is open (Part S §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptNameMode {
    NewScript,
    NewFolder,
    Rename,
}

pub(crate) fn script_name_title(mode: ScriptNameMode) -> &'static str {
    match mode {
        ScriptNameMode::NewScript => "Nový skript",
        ScriptNameMode::NewFolder => "Nová složka",
        ScriptNameMode::Rename => "Přejmenovat",
    }
}

pub(crate) fn script_delete_text(name: &str, is_dir: bool) -> String {
    let kind = if is_dir { "složku" } else { "skript" };
    format!("Smazat {kind} {name}? Akce je nevratná (maže se z disku, ne do koše).")
}

/// Part S §4's resolved simplification: when the target IS the dirty-bound
/// file, the delete confirm carries a second line instead of stacking a
/// discard confirm in front of it — one modal, both facts.
pub(crate) fn script_delete_dirty_line() -> &'static str {
    "Skript má neuložené změny v editoru."
}
```

```rust
    /// Part S §4: ONE dialog for new script / new folder / rename. The
    /// Skript↔Složka choice is a radio inside the NewScript/NewFolder pair
    /// (`mode` is what the radio flips). `parent_rel` is `""` at the root;
    /// `target_rel` is the entry being renamed (empty for creates).
    ScriptName {
        mode: ScriptNameMode,
        parent_rel: String,
        target_rel: String,
        is_dir: bool,
        field: Entity<TextField>,
        error: Option<String>,
    },
    /// Part S §4/§7.9: irreversible, and folders only when empty.
    ScriptDeleteConfirm { rel: String, is_dir: bool, dirty_bound: bool, error: Option<String> },
```

`modal_confirm_kind`:

```rust
        // Policy clause (a): confirm creates/renames a FILE and runs
        // nothing against the database. Routed through the free fn so the
        // table and its test share ONE source (Step 1).
        ModalState::ScriptName { .. } => script_name_confirm_kind(),
        // §3-novela's substance is IRREVERSIBILITY, not SQL: the button is
        // the last gate before an unrecoverable disk delete.
        | ModalState::ScriptDeleteConfirm { .. } => ModalConfirmKind::Ignore,
```

with `ModalConfirmKind::ScriptName` added to the enum and routed in `on_modal_confirm` to `self.confirm_script_name(cx)`. Esc allow-list (`on_cancel_query`): both are `true` (no secret typed, nothing dispatched).

- [ ] **Step 6: The dispatchers + binding fixups**

Replace Task 7's remaining grouped placeholder arm in `on_tree_event` with the four real arms (and DELETE the placeholder):

```rust
            TreeEvent::ScriptRunFile { rel } => self.run_script_from_library(rel.clone(), cx),
            TreeEvent::ScriptCreate { parent_rel } => {
                self.open_script_name_modal(
                    connections_ui::ScriptNameMode::NewScript,
                    parent_rel.clone(),
                    String::new(),
                    false,
                    cx,
                )
            }
            TreeEvent::ScriptRename { rel, is_dir } => self.open_script_name_modal(
                connections_ui::ScriptNameMode::Rename,
                String::new(),
                rel.clone(),
                *is_dir,
                cx,
            ),
            TreeEvent::ScriptDelete { rel, is_dir } => {
                let dirty_bound = self.binding_targets(rel) && self.script_dirty_flag;
                self.modal = Some(connections_ui::ModalState::ScriptDeleteConfirm {
                    rel: rel.clone(),
                    is_dir: *is_dir,
                    dirty_bound,
                    error: None,
                });
                self.modal_needs_focus = true;
                cx.notify();
            }
```

```rust
    /// Whether the current binding points at this library rel — the fixup
    /// predicate for rename (updates `binding.path`) and delete (clears the
    /// binding, and adds the §4 second line to the confirm).
    fn binding_targets(&self, rel: &str) -> bool {
        let (Some(b), Some(root)) = (&self.script_binding, self.effective_scripts_root()) else {
            return false;
        };
        crate::scripts::resolve_rel(&root, rel).is_ok_and(|p| p == b.path)
    }
```

Confirm handlers — every fs mutation goes through `scripts.rs` in `cx.background_spawn`, then rescans on success; the error lands in the modal's `error` field, never only in the status line:

```rust
    fn confirm_script_name(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptName {
            mode, parent_rel, target_rel, is_dir, field, ..
        }) = &self.modal
        else {
            return;
        };
        let (mode, parent_rel, target_rel, is_dir) =
            (*mode, parent_rel.clone(), target_rel.clone(), *is_dir);
        let name = field.read(cx).text();
        let Some(root) = self.effective_scripts_root() else {
            self.set_script_name_error("nastavte složku skriptů v Nastavení".to_string(), cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let job = (root.clone(), parent_rel.clone(), target_rel.clone(), name.clone());
            let result: Result<String, String> = cx
                .background_spawn(async move {
                    let (root, parent_rel, target_rel, name) = job;
                    match mode {
                        connections_ui::ScriptNameMode::NewScript => {
                            crate::scripts::create_script(&root, &parent_rel, &name)
                        }
                        connections_ui::ScriptNameMode::NewFolder => {
                            crate::scripts::create_folder(&root, &parent_rel, &name)
                        }
                        connections_ui::ScriptNameMode::Rename => {
                            crate::scripts::rename_entry(&root, &target_rel, &name, is_dir)
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(new_rel) => {
                    // Rename fixup: the binding holds an ABSOLUTE path, so
                    // it must be re-pointed at the new one (§4). The root
                    // is resolved into a LOCAL first — `binding_targets`
                    // and `effective_scripts_root` both borrow `view`
                    // immutably, so neither may run while `script_binding`
                    // is mutably borrowed.
                    let retarget = mode == connections_ui::ScriptNameMode::Rename
                        && view.binding_targets(&target_rel);
                    let root_now = view.effective_scripts_root();
                    if retarget {
                        if let (Some(b), Some(root)) = (&mut view.script_binding, root_now) {
                            if let Ok(p) = crate::scripts::resolve_rel(&root, &new_rel) {
                                b.path = p;
                            }
                        }
                    }
                    let name = new_rel.rsplit('/').next().unwrap_or(&new_rel).to_string();
                    view.status = match mode {
                        connections_ui::ScriptNameMode::Rename => format!("přejmenováno: {name}"),
                        _ => format!("skript vytvořen: {name}"),
                    };
                    view.close_modal(cx);
                    view.start_scripts_scan(cx);
                }
                Err(e) => view.set_script_name_error(e, cx),
            });
        })
        .detach();
    }

    fn confirm_script_delete(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptDeleteConfirm { rel, is_dir, .. }) = &self.modal
        else {
            return;
        };
        let (rel, is_dir) = (rel.clone(), *is_dir);
        let Some(root) = self.effective_scripts_root() else {
            return;
        };
        let was_bound = self.binding_targets(&rel);
        cx.spawn(async move |this, cx| {
            let job = (root.clone(), rel.clone());
            let result = cx
                .background_spawn(async move {
                    let (root, rel) = job;
                    crate::scripts::delete_entry(&root, &rel, is_dir)
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(()) => {
                    if was_bound {
                        // §4: the binding's file is gone — clear it. The
                        // editor TEXT stays (the user may still want it);
                        // the „ •" cannot lie about a file that no longer
                        // exists.
                        view.script_binding = None;
                    }
                    let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                    view.status = format!("smazáno: {name}");
                    view.close_modal(cx);
                    view.start_scripts_scan(cx);
                }
                Err(e) => {
                    if let Some(connections_ui::ModalState::ScriptDeleteConfirm { error, .. }) =
                        &mut view.modal
                    {
                        *error = Some(e);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
```

There is exactly ONE root resolver in the phase (`effective_scripts_root`, Task 7). If the borrow checker pushes back here, hoist the local higher — never add a second resolver, and never inline `self.workspace_root`/`config.scripts_dir` logic at a call site.

`open_script_name_modal` builds the `TextField` (`TextField::form_field`, prefilled with the current name for `Rename`), the Skript/Složka radio flips `mode` between `NewScript`/`NewFolder` for creates, and the panel renders `error` inline — the „error stays in the modal" precedent.

- [ ] **Step 7: Manual verification**

`%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`, connected to a writable database, library configured:
1. `+` on the root → „Nový skript" → `trzby` → creates `trzby.sql`, status „skript vytvořen: trzby.sql", tree refreshed.
2. `+` with the Složka radio → „prod" → folder appears, `+` inside it creates a nested script.
3. `✎` rename `trzby.sql` → `trzby-2025` → `trzby-2025.sql`, status „přejmenováno: …"; if it was bound, the caption follows without becoming dirty.
4. `✕` on a non-empty folder → „složka není prázdná — smažte nejdřív její obsah", modal stays open.
5. `✕` on the dirty-bound file → the confirm carries „Skript má neuložené změny v editoru."; „Smazat" removes it, the binding clears, the editor text stays.
6. Edit a bound script WITHOUT saving, then `▶` on it → the confirm modal's statement count matches the FILE, not the buffer; run → the run log shows the file's statements. Confirm the file on disk is unchanged (no auto-save).
7. Enter on the delete confirm does nothing; Enter in the name dialog confirms.

- [ ] **Step 8: Gate + commit**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: PASS, zero warnings; `grep -rn "tato akce zatím není dostupná" crates/` returns nothing.

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: script create/rename/delete modals + library run via factored open_script_run_modal (workspace T9)"
```

---

### Task 10: Sweep — as-built docs, memory, version 0.22.0, full gates

**Files:**
- Modify: `Cargo.toml` (root, `[workspace.package] version`)
- Modify: `docs/superpowers/specs/drafts/workspace-folder-design.md` (as-built deltas only)
- Modify: `docs/superpowers/plans/2026-08-25-scripts-library.md` (banner: superseded)
- Modify: `C:\Users\tomas\.claude\projects\D--workspace-home-db\memory\db-client-project.md` and its `MEMORY.md` index line

Batch 5, ALONE. Everything else must be merged and green before this starts.

**Interfaces:**
- Consumes: every task's landed shape — this task ASSERTS, it does not implement.
- Produces: `v0.22.0` on `feature/scripts-library`, an as-built design doc, an updated memory entry.

- [ ] **Step 1: Placeholder / dead-code sweep**

```bash
cd D:/workspace/home/db/.claude/worktrees/scripts
grep -rn "tato akce zatím není dostupná" crates/            # Task 7's placeholder — must be GONE
grep -rn "DARK UNTIL TASK 7" crates/                        # Task 3's markers — must be GONE
grep -rn "allow(dead_code)" crates/dbc-ui crates/dbc-state crates/dbc-mcp
grep -rn "TODO\|FIXME\|unimplemented!\|todo!" crates/dbc-ui/src/scripts.rs crates/dbc-state/src/workspace.rs crates/dbc-state/src/fsutil.rs
```

Expected: the first two return NOTHING. Every surviving `allow(dead_code)` must have a named removal owner in the same comment (Global Constraints) — if one names a task in THIS phase, that task did not finish and this step stops.

- [ ] **Step 2: The shared-rails audit (the review-blocking one)**

```bash
grep -rn "read_dir" crates/dbc-state/src crates/dbc-ui/src/scripts.rs
grep -rn "eq_ignore_ascii_case" crates/dbc-state/src crates/dbc-ui/src/scripts.rs
grep -rn "\.tmp" crates/dbc-state/src crates/dbc-ui/src/scripts.rs
grep -rn "sync_all" crates/
```

Expected, and each is a hard gate:
1. `read_dir` in `dbc-state`: exactly TWO sites — `fsutil::entry_exists_ci` (the probe) and `workspace::classify` (the emptiness check, which reads names only and never opens anything under `.git/`).
2. `eq_ignore_ascii_case` appears NOWHERE in a filename comparison (the Unicode-aware `to_lowercase` probe is the only one; `.sql` extension checks against a pure-ASCII literal are fine and may legitimately remain in `main.rs`).
3. Exactly ONE `.tmp` + `sync_all` + `rename` block in the whole workspace-touching surface: `fsutil::write_atomic`. `AppConfig::save`'s pre-existing writer may stay as-is (it predates this phase and is not a writer into a user-chosen folder) — if it was ALSO migrated onto the rail, note that in the as-built doc; if a THIRD one exists anywhere, this phase is not done.

- [ ] **Step 3: The security sweep**

```bash
grep -rn "password\|heslo\|secret" crates/dbc-state/src/workspace.rs crates/dbc-state/src/fsutil.rs
grep -rn "git2\|notify\|walkdir\|rfd" Cargo.toml crates/*/Cargo.toml
grep -rn "\.git" crates/dbc-state/src crates/dbc-ui/src/scripts.rs
```

Expected:
1. `workspace.rs`/`fsutil.rs` mention secrets only in COMMENTS and in the `.gitignore` template's advice text — no field, no serialization, no logging.
2. No new dependency: `git2`, `notify`, `walkdir` (except GPUI's transitive one), `rfd` all absent.
3. `.git` appears only as a name being SKIPPED (`classify`'s dot-entry rule, the scan's dot-dir rule) — never opened, never parsed (§W6.4, permanent).
4. `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state no_password_field_serialized no_plaintext_secret_is_written_outside_vault_bin` — both green.

- [ ] **Step 4: As-built doc deltas**

Append a short „Jak to nakonec je (as-built)" section to `docs/superpowers/specs/drafts/workspace-folder-design.md` recording the deviations this plan made from the design text, so the next phase reads the truth and not the intent:

1. **Task ordering inverted** — workspace lane before the scripts flip, so `effective_scripts_root` landed complete (both arms) in one seam instead of §W9's profile-only stub.
2. **`Resolution::Broken` carries `{ root: Option<PathBuf>, reason: String }`**, not §W2's sketch `{ pointer, root, reason }`: the pointer path is a process-wide constant (`workspace::pointer_path()`), and `root` must be optional because an unparsable pointer names no folder.
3. **The blocked start uses `blocked_paths`**, not profile paths — §W4 said "starts with an EMPTY default config" but not which paths `AppView` then holds. Answer: paths inside the unusable workspace (or a never-created sentinel folder), so a stray save fails loudly instead of overwriting a profile the user did not choose.
4. **`apply_context` also clears `conn_url`** (the CLI-arg root). §W3.1 said "the active connection is disconnected"; the CLI session belongs to the old context, so it goes with it — and per the sidebar design it cannot come back.
5. **Result tabs SURVIVE a context swap.** §W3.4 enumerates what the swap replaces and does not list tabs; they hold results already produced, exactly like after a connection switch today. Recorded rather than silently decided.
6. **Init always copies from the PROFILE**, never from an active workspace — which is consistent, because §W3 offers no folder picker in workspace mode (the route to a second workspace is profile-and-back).
7. **`ModalState::WorkspaceConfirm` is ONE variant with a `WorkspaceConfirmMode`** covering init / adopt / back-to-profile, so the gate, the „Aktivní připojení bude odpojeno." line and the Enter-inert policy exist once.
8. **dbc-mcp's broken-pointer refusal is scoped to the paths the command needs** (`--help` needs none, `setup` needs the vault, `serve` needs both) — §W7 said "exits with the error" without saying whether explicit overrides rescue it. They do.
9. **`context_switch_blocked` grew its dirty-script arm in Task 8**, not Task 5, because `script_binding` did not exist yet. One gate function throughout.

Also flip `docs/superpowers/plans/2026-08-25-scripts-library.md`'s banner to point here (it may already carry one — verify the wording names THIS file).

- [ ] **Step 5: Memory update**

Rewrite the project memory entry at `C:\Users\tomas\.claude\projects\D--workspace-home-db\memory\db-client-project.md` (and its one-line summary in `MEMORY.md`) to record: workspace folder + scripts library shipped at **v0.22.0**; the pointer file `%APPDATA%\dbc\workspace.toml` and the marker `dbc-workspace.toml`; the three shared fs rails in `dbc_state::fsutil`; git permanently external (no dep, no subprocess, no `.git/` read); `history.sqlite` stays machine-local; the scripts root seam `effective_scripts_root` (workspace ⇒ `<workspace>/scripts`, profile ⇒ `AppConfig.scripts_dir`, which is inert in workspace mode). Keep the existing deferred-work list and append nothing that this phase actually closed.

- [ ] **Step 6: Release-notes disclosures (§W7)**

Record, wherever the repo keeps release notes (if there is no such file, put them in the as-built section from Step 4 — do NOT create a new doc for this):
- `vault.bin` **cannot be merged** by git: a conflict is resolved by taking one side wholesale, and the losing side's newly-added passwords must be re-entered. The app cannot and deliberately does not help.
- `tool_paths` travels with the workspace and may point at machine-A paths; backup/restore then errors clearly until fixed in Settings.
- `history.sqlite` does not travel.
- There is **no app-exit dirty guard** for a bound script (no exit interception exists app-wide) — same posture as today's editor text.

- [ ] **Step 7: Version bump**

In the root `Cargo.toml`, `[workspace.package]`: `version = "0.21.0"` → `version = "0.22.0"`. Verify first that `0.22.0` is still free on `main` (house convention):

```bash
git log --oneline main -20 | grep -i "0.22" || echo "0.22.0 free"
```

- [ ] **Step 8: Full gates**

All via `%USERPROFILE%\.cargo\bin\cargo.exe`:
1. `test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` — green, zero warnings.
2. `build --workspace` — zero warnings.
3. `build --workspace --release` — zero warnings.
4. `test --workspace` — green (the bare-workspace run is allowed only here).

Expected: all green; the window title reads `dbc v0.22.0`.

- [ ] **Step 9: End-to-end smoke (the multi-machine story, §W7, on one machine)**

1. Start in profile mode with real connections and a saved password. Settings → „Použít složku…" → a fresh empty folder → „Rozumím, vytvořit".
2. `git init` + `git add -A` + `git commit` **in a terminal, outside the app** — confirm the app noticed nothing and offers nothing git-related anywhere in its UI.
3. Add a script via `+`, save it with Ctrl+S, run it with `▶`.
4. Close the app, `git clone` the folder to a second path, restart, Settings → „Přejít na lokální profil", then „Použít složku…" → the CLONE → classified as adopt („Otevřít pracovní prostor") → the same connections, favourites, theme, scripts tree appear; the first connect prompts for the SAME master password.
5. Rename the clone folder on disk while the app is closed; restart. Expected: the blocking „Pracovní prostor nenalezen" modal, Esc/Enter inert, an empty connection list behind it, and „Najít složku…" recovering it.
6. `dbc-mcp --help` and a bare `dbc-mcp` run against the same pointer — the workspace's config/vault are used; with the folder renamed, a Czech refusal on stderr and a non-zero exit.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock docs/superpowers/specs/drafts/workspace-folder-design.md docs/superpowers/plans/2026-08-25-scripts-library.md
git commit -m "chore: v0.22.0 — workspace folder + scripts library (workspace T10)"
```

---
