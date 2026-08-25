# Scripts Library (Bruno model) Implementation Plan

> **SCOPE WIDENED 2026-08-25 — do not execute this plan as-is.** The phase now covers a full git-versioned **workspace folder** (connections, settings, vault, prefs, scripts); the binding design is `docs/superpowers/specs/drafts/workspace-folder-design.md`. T1/T2 below are DONE (branches `sc-t1-config`, `sc-t2-fsmod`); T3–T6 remain valid with the `effective_scripts_root` seam (design §W8); a rewritten plan will supersede this file after design review.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers per task, a **sonnet** adversarial review per task, and a **default-model** final review once all tasks land (house staffing convention). NO docker, NO external server anywhere in this phase: every test is a pure `#[test]` over plain data or a `tempfile` directory.

**Goal:** A Bruno-style scripts library — plain `.sql` files in one user-chosen folder, shown as a new pinned „Skripty" section in the sidebar: open into the (global) editor with Ctrl+S save + dirty tracking, create/rename/delete files and folders, and run a file through the UNCHANGED G12 script-run confirm flow. Git stays 100% external.

**Architecture:** Three layers: (1) a **pure/std-fs module** `dbc-ui/src/scripts.rs` (scan with caps + symlink/traversal rails, name validation, atomic fs ops — fully tempfile-tested); (2) an **additive sidebar section** — new `SidebarRow`/`OuterId` variants + a `ScriptsListState` slot on `SchemaTree`, emitted by the pure `flatten_sidebar` behind an `Option` parameter (dark until the flip); (3) **main.rs wiring** — background scan dispatch, a `ScriptBinding` on the single global editor (there ARE no editor tabs — tabs are result tabs; the binding adapts the brief's intent to the real architecture), Ctrl+S, and a factored reuse of `start_script_pick`'s confirm-modal continuation so the run path cannot fork policy.

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`), **no new dependencies** (no `notify`, no `walkdir`, no `rfd` — hand-rolled `read_dir` + client-side `.sql` checks are the house precedent).

**Spec:** `docs/superpowers/specs/drafts/scripts-library-design.md` — binding; user scope decision (pure Bruno, git external, no git UI ever) restated there. Line numbers below were taken on branch `feature/scripts-library` at v0.21.0 (9589df6) — **always re-locate by symbol, never by line number**.

**Resolved deviations from the design doc** (called out inline at their tasks):

1. **The root „+" icon lands in T6, not T4** — it opens the `ScriptName` modal, which only exists in T6; T4's root row carries only `⟳`.
2. **`TreeEvent` variants land incrementally with their handlers** (T4: `ScriptsRefresh`/`OpenScriptsSettings`; T5: `ScriptOpen`; T6: `ScriptCreate`/`ScriptRename`/`ScriptDelete`/`ScriptRunFile`): `main.rs::on_tree_event` matches exhaustively, so a variant and its arm must share a task (sidebar plan deviation-9 precedent).
3. **`ScriptNotice` carries a small `ScriptNoticeAction` enum** (`None`/`OpenSettings`/`Retry`) instead of the design's single `open_settings: bool` — the error row needs Retry semantics and a bool pair would be two flags with an illegal state.
4. **No `PendingScriptAction::DeleteBound`** — the design already resolved delete-of-dirty-bound to a second line INSIDE the delete confirm modal (one modal, both facts); the discard-guard enum stays three variants.
5. **`ModalState::ScriptName` has no separate `mode` field** (the design §4 sketch listed one) — rename-vs-create is exactly `target_rel: Option<String>`; a second field that could disagree with it is a bug shape, not an API (fetch_database_list-deviation precedent).

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags (bare workspace builds only in T7's final gate).
- **Zero warnings** in plain AND test builds, debug AND release, for every crate touched. New pub items get doc comments; no `#[allow(dead_code)]` without a named removal owner.
- GPUI pin `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no upgrade, no new primitives; `uniform_list` for all sidebar rows; **no extension-filter API exists** in its file dialogs — `.sql` checks are client-side (G12 precedent, main.rs:2626).
- **Git integration is permanently external** (user decision): no git dependency, no git subprocess, no git UI, no credentials. If a step seems to want git status — stop, it is out of scope forever.
- **Never auto-execute:** opening a script only reads text; save/rename/delete never touch a database; running goes ONLY through the existing G12 confirm modal + runner gates.
- **No new secrets:** the only new persisted datum is `scripts_dir` (a path) in config.toml — ToolPaths posture.
- **Caps (design §7):** `SCRIPTS_ENTRY_CAP = 2000` (disclosure Notice — the 2000-db precedent), `SCRIPTS_DEPTH_CAP = 12` (disclosure Notice), `SCRIPT_OPEN_CAP = 1 MiB` (editor open only; ▶ runs stream uncapped through the G12 splitter), `SCRIPT_NAME_CAP = 80`.
- **Symlinks are skipped at scan** (never descended, never listed) — the tree can never leave the chosen root; `resolve_rel` re-rejects traversal as defense in depth.
- Czech user-facing strings exactly as quoted in the tasks below; errors use the `"error: …"` status prefix; notices reuse the `…`/`—` idiom.
- **Speed search filters LOADED content only** — script rows filter in the pure flatten layer; typing never triggers a scan.
- **Single-writer serialized files:** `main.rs`, `schema_tree.rs`, `connections_ui.rs` — T4 → T5 → T6 run strictly in sequence; T1 ∥ T2 may run in parallel (T2's only main.rs touch is the one-line `mod scripts;`, landed in T2's own commit — do not batch T1/T2 with anything else that edits main.rs).
- **Merge gate** (every task, and finally T7): `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` green, zero warnings. dbc-mcp reads the same config.toml and must stay green with a NONE diff (regression canary).
- **Versioning:** T7 bumps `[workspace.package] version` (root `Cargo.toml`, currently `0.21.0`) to **0.22.0** (re-verify free on main at merge time, house convention).

### Task dependency graph

| Task | Name | Depends on | Files | Batch |
|---|---|---|---|---|
| T1 | dbc-state: `scripts_dir` + back-compat tests | — | `dbc-state/src/config.rs` | A (parallel) |
| T2 | `scripts.rs`: scan/validate/resolve/fs ops + tempfile suite | — | `dbc-ui/src/scripts.rs` (new), `dbc-ui/src/main.rs` (one `mod` line) | A (parallel) |
| T3 | schema_tree ADDITIVE: rows, `OuterId`, `ScriptsListState`, `script_rows`, expand plumbing (dark: `scripts: None`) | T2 | `dbc-ui/src/schema_tree.rs` | B |
| T4 | FLIP: settings row „Složka skriptů", scan dispatch, section live, ⟳/notice events | T1, T3 | `dbc-ui/src/{main.rs,connections_ui.rs,schema_tree.rs}` | C (SOLO) |
| T5 | Editor binding: open, Ctrl+S save/save-as, caption strip, discard guard, palette entry | T4 | `dbc-ui/src/{main.rs,schema_tree.rs,history_panel.rs,palette.rs}` | D (SOLO) |
| T6 | Mutations + run: name/delete modals, fs dispatch, binding fixups, ▶ → factored G12 confirm | T5 | `dbc-ui/src/{main.rs,connections_ui.rs,schema_tree.rs}` | E (SOLO) |
| T7 | Sweep: docs as-built, memory, v0.22.0, full gates + smoke | all | root `Cargo.toml`, docs | last |

Suggested batches: **{T1, T2}** parallel → **{T3}** → **{T4}** → **{T5}** → **{T6}** → **{T7}**.

---

### Task 1 (T1): dbc-state — `AppConfig.scripts_dir`

**Files:**
- Modify: `crates/dbc-state/src/config.rs` (`AppConfig` at :119-131 + its `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `AppConfig.scripts_dir: Option<String>` — read by T4 (`main.rs` scan dispatch, `connections_ui.rs` settings row), T5 (binding/save-as), T6 (fs ops root).

- [ ] **Step 1: Write the failing tests.** In `config.rs`'s existing `mod tests`, next to `tool_paths_defaults_to_none_when_absent_from_old_config`:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state scripts_dir`
Expected: FAIL to compile — `no field 'scripts_dir' on type 'AppConfig'`.

- [ ] **Step 3: Add the field.** In `AppConfig` (config.rs:119-131), after `tool_paths`:

```rust
    /// Scripts library (Bruno model, design §2): absolute path of the
    /// user-chosen folder of plain `.sql` files. `None` = feature dormant
    /// (the sidebar section points at Settings). A path, not a secret.
    /// Git integration for this folder is deliberately EXTERNAL (user
    /// decision 2026-08-25) — the app never reads or writes anything
    /// git-related about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_dir: Option<String>,
```

- [ ] **Step 4: Run to verify pass + gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state` then `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: all green, zero warnings (dbc-mcp diff: NONE — it deserializes the same struct; the additive default keeps it green).

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-state/src/config.rs
git commit -m "feat: AppConfig.scripts_dir additive field (scripts T1)"
```

---

### Task 2 (T2): `crates/dbc-ui/src/scripts.rs` — pure model + fs helpers

**Files:**
- Create: `crates/dbc-ui/src/scripts.rs`
- Modify: `crates/dbc-ui/src/main.rs` — ONE line: add `mod scripts;` next to the existing `mod` declarations (grep `mod schema_tree;` near the top). Nothing else in main.rs.

**Interfaces:**
- Consumes: nothing (std only).
- Produces (used by T3–T6):

```rust
pub struct ScriptEntry { pub rel: String, pub is_dir: bool, pub depth: usize }
pub struct ScriptScan { pub entries: Vec<ScriptEntry>, pub truncated: bool, pub depth_clipped: bool }
pub const SCRIPTS_ENTRY_CAP: usize = 2000;
pub const SCRIPTS_DEPTH_CAP: usize = 12;
pub const SCRIPT_OPEN_CAP: u64 = 1_048_576;
pub const SCRIPT_NAME_CAP: usize = 80;
pub fn scan_scripts(root: &Path) -> Result<ScriptScan, String>;
pub fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String>;
pub fn validate_script_name(name: &str, is_file: bool) -> Result<String, String>;
pub fn create_script(root: &Path, parent_rel: &str, name: &str) -> Result<String, String>;
pub fn create_folder(root: &Path, parent_rel: &str, name: &str) -> Result<String, String>;
pub fn rename_entry(root: &Path, rel: &str, new_name: &str, is_dir: bool) -> Result<String, String>;
pub fn delete_entry(root: &Path, rel: &str, is_dir: bool) -> Result<(), String>;
pub fn write_script(path: &Path, text: &str) -> Result<(), String>;
pub fn read_script(path: &Path) -> Result<String, String>;
```

- [ ] **Step 1: Write the module with its test suite in one file** (TDD note: with a brand-new module the "failing test" state is the whole file failing to compile without the impls — write tests first in the file, `cargo test` to see them fail to resolve, then fill the impls; the two sub-steps below are that cycle). Create `crates/dbc-ui/src/scripts.rs`:

```rust
//! Scripts library (Bruno model, design: scripts-library-design.md).
//! Pure model + std-fs helpers for the user-chosen `.sql` folder.
//! SECURITY (design §7): every path is built by joining VALIDATED single
//! components onto the root; symlinks are skipped at scan; nothing here
//! ever executes SQL or touches a database connection.

use std::fs;
use std::path::{Path, PathBuf};

/// One entry of the scanned library, in DISPLAY order (depth-first;
/// within a directory: folders first, then files, each ordered by
/// case-insensitive name). `rel` uses '/' separators on all platforms —
/// stable expand keys and event payloads; `resolve_rel` maps back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptEntry {
    pub rel: String,
    pub is_dir: bool,
    /// 0 = direct child of the root.
    pub depth: usize,
}

/// A completed scan. `truncated`/`depth_clipped` drive the disclosure
/// Notice rows (2000-db precedent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptScan {
    pub entries: Vec<ScriptEntry>,
    pub truncated: bool,
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
    if full.starts_with('.') || full.ends_with('.') || full.starts_with(' ') || full.ends_with(' ') {
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
    let rd = fs::read_dir(parent).map_err(|e| format!("nelze číst složku {}: {e}", parent.display()))?;
    for ent in rd {
        let ent = ent.map_err(|e| format!("nelze číst složku {}: {e}", parent.display()))?;
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
        let mut rd = fs::read_dir(&p).map_err(|e| format!("nelze číst složku {}: {e}", p.display()))?;
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
            "soubor je příliš velký pro editor (limit 1 MiB) — spusťte jej jako skript".to_string()
        );
    }
    let bytes = fs::read(path).map_err(|e| format!("soubor nelze otevřít: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "soubor není platné UTF-8".to_string())
}
```

- [ ] **Step 2: Append the test module** (same file):

```rust
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
        touch(&r.join("q.sql"));
        touch(&r.join("q.SQL"));
        fs::write(r.join("readme.md"), b"x").unwrap();
        fs::write(r.join("noext"), b"x").unwrap();
        fs::create_dir(r.join(".git")).unwrap();
        touch(&r.join(".git").join("hidden.sql"));
        let scan = scan_scripts(r).unwrap();
        let rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, vec!["q.SQL", "q.sql"]);
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
        for bad in ["..", "a/../b", "a/..", "/abs", "a//b", "C:\\x", "\\\\srv\\share", "a\u{0}b", "."] {
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
```

- [ ] **Step 3: Register the module.** In `crates/dbc-ui/src/main.rs`, next to the existing `mod schema_tree;` declaration block, add:

```rust
mod scripts;
```

- [ ] **Step 4: Run the suite**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui scripts::`
Expected: all tests above PASS; then the full gate `test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` green, zero warnings (every pub item above is doc-commented — a bare `pub fn` would warn under the house lint posture).

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/scripts.rs crates/dbc-ui/src/main.rs
git commit -m "feat: scripts.rs — scan/validate/fs ops with safety rails (scripts T2)"
```

---

### Task 3 (T3): schema_tree — additive scripts section (dark)

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs` — `SidebarRow` (:576), `OuterId` (:601), new state types near `DbListState` (:629), new pure fn near `flatten_sidebar` (:920), `toggle_outer` (:1556), `handle_chevron` (:1724), `handle_single_click` (:1776), `handle_double_click` (:1630), `row_is_expanded` (:1801), `row_in_active_scope` (:693), the `Render` impl's `flatten_sidebar` call (:1886) and row match arms, plus the existing flatten test module.

**Interfaces:**
- Consumes: `crate::scripts::{ScriptEntry, ScriptScan}` (T2).
- Produces (used by T4–T6):

```rust
pub enum ScriptsListState { NotLoaded, Loading { generation: u64 }, Error(String),
    Loaded { entries: Vec<crate::scripts::ScriptEntry>, truncated: bool, depth_clipped: bool } }
// New SidebarRow variants:
//   ScriptsRoot,
//   ScriptFolder { rel: String },
//   ScriptFile { rel: String },
//   ScriptNotice { text: String, action: ScriptNoticeAction },
pub enum ScriptNoticeAction { None, OpenSettings, Retry }
// New OuterId variants: Scripts, ScriptFolder(String)  (presence = EXPANDED, lazy default)
pub fn script_rows(state: &ScriptsListState, dir_configured: bool,
    outer_expanded: &HashSet<OuterId>, filter: &str) -> Vec<SidebarFlatRow>;
// flatten_sidebar gains a trailing parameter:
//   scripts: Option<(&ScriptsListState, bool)>,   // None = section absent (dark until T4)
// SchemaTree methods (state carrier; wired by T4):
pub fn begin_scripts_scan(&mut self, cx: &mut Context<Self>) -> u64;
pub fn apply_scripts_scan(&mut self, generation: u64, result: Result<crate::scripts::ScriptScan, String>, cx: &mut Context<Self>);
pub fn set_scripts_dir_present(&mut self, present: bool, cx: &mut Context<Self>);
pub fn clear_scripts(&mut self, cx: &mut Context<Self>);
```

Czech literals (binding, design §3.4): section label `"Skripty"`; `"složka skriptů není nastavena — klikněte pro Nastavení"`; `"Načítám skripty…"`; `"žádné skripty (*.sql)"`; `"… zobrazeno prvních 2000 položek — zmenšete knihovnu skriptů"`; `"… některé podsložky jsou příliš hluboko (limit 12 úrovní)"`.

- [ ] **Step 1: Write the failing tests** (in the existing schema_tree test module, next to the `flatten_sidebar` tests):

```rust
    fn loaded(entries: Vec<crate::scripts::ScriptEntry>) -> ScriptsListState {
        ScriptsListState::Loaded { entries, truncated: false, depth_clipped: false }
    }
    fn se(rel: &str, is_dir: bool, depth: usize) -> crate::scripts::ScriptEntry {
        crate::scripts::ScriptEntry { rel: rel.into(), is_dir, depth }
    }

    #[test]
    fn scripts_root_collapsed_emits_only_header() {
        let rows = script_rows(&loaded(vec![se("a.sql", false, 0)]), true, &HashSet::new(), "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "Skripty");
        assert!(rows[0].3, "root must be expandable");
    }

    #[test]
    fn scripts_unconfigured_expanded_points_at_settings() {
        let mut ex = HashSet::new();
        ex.insert(OuterId::Scripts);
        let rows = script_rows(&ScriptsListState::NotLoaded, false, &ex, "");
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[1].0,
            SidebarRow::ScriptNotice { action: ScriptNoticeAction::OpenSettings, .. }
        ));
        assert_eq!(rows[1].2, "složka skriptů není nastavena — klikněte pro Nastavení");
    }

    #[test]
    fn scripts_error_row_is_retryable_and_loading_is_not() {
        let mut ex = HashSet::new();
        ex.insert(OuterId::Scripts);
        let rows = script_rows(&ScriptsListState::Error("boom".into()), true, &ex, "");
        assert_eq!(rows[1].2, "error: boom");
        assert!(matches!(&rows[1].0, SidebarRow::ScriptNotice { action: ScriptNoticeAction::Retry, .. }));
        let rows = script_rows(&ScriptsListState::Loading { generation: 1 }, true, &ex, "");
        assert_eq!(rows[1].2, "Načítám skripty…");
        assert!(matches!(&rows[1].0, SidebarRow::ScriptNotice { action: ScriptNoticeAction::None, .. }));
    }

    #[test]
    fn scripts_collapsed_folder_hides_subtree_expanded_shows_it() {
        let entries = vec![
            se("rep", true, 0),
            se("rep/a.sql", false, 1),
            se("repx.sql", false, 0),
        ];
        let mut ex = HashSet::new();
        ex.insert(OuterId::Scripts);
        let rows = script_rows(&loaded(entries.clone()), true, &ex, "");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        // "repx.sql" must NOT be swallowed by the "rep/" prefix (slash-aware skip).
        assert_eq!(labels, vec!["Skripty", "rep", "repx.sql"]);
        ex.insert(OuterId::ScriptFolder("rep".into()));
        let rows = script_rows(&loaded(entries), true, &ex, "");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(labels, vec!["Skripty", "rep", "a.sql", "repx.sql"]);
        assert_eq!(rows[2].1, 2, "file depth = 1 (root) + entry.depth");
    }

    #[test]
    fn scripts_caps_emit_disclosure_notices_and_empty_root_says_so() {
        let mut ex = HashSet::new();
        ex.insert(OuterId::Scripts);
        let state = ScriptsListState::Loaded { entries: vec![], truncated: true, depth_clipped: true };
        let rows = script_rows(&state, true, &ex, "");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Skripty",
                "žádné skripty (*.sql)",
                "… zobrazeno prvních 2000 položek — zmenšete knihovnu skriptů",
                "… některé podsložky jsou příliš hluboko (limit 12 úrovní)",
            ]
        );
    }

    #[test]
    fn scripts_filter_keeps_matches_and_their_ancestors_never_fetches() {
        let entries = vec![
            se("rep", true, 0),
            se("rep/prodeje.sql", false, 1),
            se("rep/jine.sql", false, 1),
            se("other.sql", false, 0),
        ];
        // Filter works WITHOUT the root being expanded (auto-expand under
        // filter, same posture as Inner rows) and ignores folder collapse.
        let rows = script_rows(&loaded(entries), true, &HashSet::new(), "prod");
        let labels: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(labels, vec!["Skripty", "rep", "prodeje.sql"]);
        // No match and root label misses -> whole section drops.
        let rows = script_rows(&loaded(vec![se("x.sql", false, 0)]), true, &HashSet::new(), "zzz");
        assert!(rows.is_empty());
    }

    #[test]
    fn flatten_sidebar_none_scripts_emits_no_scripts_rows() {
        // The dark-landing pin: with scripts = None the output must not
        // contain any Scripts* row regardless of other inputs. Reuse the
        // existing flatten_sidebar test fixture's minimal call and assert:
        // rows.iter().all(|r| !matches!(r.0, SidebarRow::ScriptsRoot
        //     | SidebarRow::ScriptFolder { .. } | SidebarRow::ScriptFile { .. }
        //     | SidebarRow::ScriptNotice { .. }))
        // (adapt the fixture the existing tests already build).
    }
```

For the last test, copy the smallest existing `flatten_sidebar` invocation in the test module and add the assertion shown in its comment — the fixture types already exist there.

- [ ] **Step 2: Run to verify failure**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui schema_tree::`
Expected: FAIL to compile — `ScriptsListState`/`script_rows`/`ScriptNoticeAction` unresolved.

- [ ] **Step 3: Implement the types + `script_rows`.** Add next to `DbListState` (:629):

```rust
/// Scripts library (design §3.3): lazy-scan state machine for the
/// „Skripty" section — same family as `DbListState`; `generation` makes
/// scans last-dispatched-wins (`apply_scripts_scan` drops mismatches).
pub enum ScriptsListState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { entries: Vec<crate::scripts::ScriptEntry>, truncated: bool, depth_clipped: bool },
}

/// What a click on a `ScriptNotice` row does (resolved deviation 3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScriptNoticeAction {
    None,
    OpenSettings,
    Retry,
}
```

Extend `SidebarRow` (:576) with the four variants (doc comments naming the design §3.3) and `OuterId` (:601) with `Scripts` and `ScriptFolder(String)` — both LAZY polarity (presence = expanded), documented on the enum's polarity note. Then the pure fn (place next to `flatten_sidebar`):

```rust
/// Scripts library (design §3.3): emits the pinned „Skripty" section.
/// Pure — never fetches; the filter path auto-expands (Inner-row
/// precedent) and keeps matches plus their ancestor folders.
pub fn script_rows(
    state: &ScriptsListState,
    dir_configured: bool,
    outer_expanded: &HashSet<OuterId>,
    filter: &str,
) -> Vec<SidebarFlatRow> {
    let mut out: Vec<SidebarFlatRow> = Vec::new();
    out.push((SidebarRow::ScriptsRoot, 0, "Skripty".to_string(), true));
    let filter_lc = filter.to_lowercase();
    let filter_active = !filter_lc.is_empty();
    if !filter_active && !outer_expanded.contains(&OuterId::Scripts) {
        return out;
    }
    let notice = |text: &str, action: ScriptNoticeAction| {
        (
            SidebarRow::ScriptNotice { text: text.to_string(), action },
            1,
            text.to_string(),
            false,
        )
    };
    if !dir_configured {
        if filter_active {
            if !name_matches("Skripty", &filter_lc) {
                out.clear();
            }
            return out;
        }
        out.push(notice(
            "složka skriptů není nastavena — klikněte pro Nastavení",
            ScriptNoticeAction::OpenSettings,
        ));
        return out;
    }
    match state {
        ScriptsListState::NotLoaded => {} // expand handler dispatches the scan
        ScriptsListState::Loading { .. } => {
            if !filter_active {
                out.push(notice("Načítám skripty…", ScriptNoticeAction::None));
            }
        }
        ScriptsListState::Error(e) => {
            if !filter_active {
                out.push(notice(&format!("error: {e}"), ScriptNoticeAction::Retry));
            }
        }
        ScriptsListState::Loaded { entries, truncated, depth_clipped } => {
            let entry_row = |e: &crate::scripts::ScriptEntry| {
                let name = e.rel.rsplit('/').next().unwrap_or(&e.rel).to_string();
                let row = if e.is_dir {
                    SidebarRow::ScriptFolder { rel: e.rel.clone() }
                } else {
                    SidebarRow::ScriptFile { rel: e.rel.clone() }
                };
                (row, 1 + e.depth, name, e.is_dir)
            };
            if filter_active {
                // Keep matches + ancestor folders; ignore collapse state.
                let dir_index: HashMap<&str, usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.is_dir)
                    .map(|(i, e)| (e.rel.as_str(), i))
                    .collect();
                let mut keep: HashSet<usize> = HashSet::new();
                for (i, e) in entries.iter().enumerate() {
                    let name = e.rel.rsplit('/').next().unwrap_or(&e.rel);
                    if name_matches(name, &filter_lc) {
                        keep.insert(i);
                        let mut rel: &str = &e.rel;
                        while let Some((parent, _)) = rel.rsplit_once('/') {
                            if let Some(&j) = dir_index.get(parent) {
                                keep.insert(j);
                            }
                            rel = parent;
                        }
                    }
                }
                if keep.is_empty() {
                    if !name_matches("Skripty", &filter_lc) {
                        out.clear();
                    }
                    return out;
                }
                for (i, e) in entries.iter().enumerate() {
                    if keep.contains(&i) {
                        out.push(entry_row(e));
                    }
                }
            } else {
                // Slash-aware prefix skip for collapsed folders.
                let mut skip_prefix: Option<String> = None;
                for e in entries {
                    if let Some(p) = &skip_prefix {
                        if e.rel.starts_with(p.as_str()) {
                            continue;
                        }
                        skip_prefix = None;
                    }
                    out.push(entry_row(e));
                    if e.is_dir && !outer_expanded.contains(&OuterId::ScriptFolder(e.rel.clone())) {
                        skip_prefix = Some(format!("{}/", e.rel));
                    }
                }
                if entries.is_empty() {
                    out.push(notice("žádné skripty (*.sql)", ScriptNoticeAction::None));
                }
                if *truncated {
                    out.push(notice(
                        "… zobrazeno prvních 2000 položek — zmenšete knihovnu skriptů",
                        ScriptNoticeAction::None,
                    ));
                }
                if *depth_clipped {
                    out.push(notice(
                        "… některé podsložky jsou příliš hluboko (limit 12 úrovní)",
                        ScriptNoticeAction::None,
                    ));
                }
            }
        }
    }
    out
}
```

(`name_matches` and `HashMap` are already in scope in this file; add the `use std::collections::HashMap;` only if the file doesn't already import it.)

- [ ] **Step 4: Thread the section through `flatten_sidebar` (dark).** Add the trailing parameter `scripts: Option<(&ScriptsListState, bool)>` to `flatten_sidebar` (:920) and, immediately AFTER the pinned Oblíbené block (:965) and BEFORE the CLI root (:967), splice:

```rust
    // Scripts library (design §1.4): pinned GLOBAL section — after
    // Oblíbené, before CLI/connection roots. None = dark (pre-flip).
    if let Some((state, dir_configured)) = scripts {
        out.extend(script_rows(state, dir_configured, outer_expanded, filter));
    }
```

Update every existing `flatten_sidebar` call site (the `Render` impl :1886 and each test) to pass `None`.

- [ ] **Step 5: State carrier + expand plumbing.** Add fields to `SchemaTree` (next to `outer_expanded` :1185): `scripts: ScriptsListState` (init `NotLoaded`), `scripts_dir_set: bool` (init `false`), `scripts_generation: u64` (init `0`). Add the four methods:

```rust
    /// Marks the scripts slot Loading and returns the generation the
    /// caller must hand back to `apply_scripts_scan` (last-dispatched-wins).
    pub fn begin_scripts_scan(&mut self, cx: &mut Context<Self>) -> u64 {
        self.scripts_generation += 1;
        self.scripts = ScriptsListState::Loading { generation: self.scripts_generation };
        cx.notify();
        self.scripts_generation
    }

    /// Applies a finished scan; a stale generation is dropped silently.
    pub fn apply_scripts_scan(
        &mut self,
        generation: u64,
        result: Result<crate::scripts::ScriptScan, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.scripts_generation {
            return;
        }
        self.scripts = match result {
            Ok(s) => ScriptsListState::Loaded {
                entries: s.entries,
                truncated: s.truncated,
                depth_clipped: s.depth_clipped,
            },
            Err(e) => ScriptsListState::Error(e),
        };
        cx.notify();
    }

    /// Pushed by main.rs whenever `config.scripts_dir` presence changes.
    pub fn set_scripts_dir_present(&mut self, present: bool, cx: &mut Context<Self>) {
        self.scripts_dir_set = present;
        cx.notify();
    }

    /// „Odebrat" in Settings: back to a dormant section.
    pub fn clear_scripts(&mut self, cx: &mut Context<Self>) {
        self.scripts = ScriptsListState::NotLoaded;
        self.scripts_dir_set = false;
        cx.notify();
    }
```

Then the compiler-guided sweep over every match that is now non-exhaustive; the T3 semantics are EXPANSION ONLY (events come later):
- `toggle_outer` (:1556): `ScriptsRoot` ⇒ toggle `OuterId::Scripts`; `ScriptFolder { rel }` ⇒ toggle `OuterId::ScriptFolder(rel)` (lazy polarity: insert = expand).
- `handle_chevron` (:1724) + `handle_double_click` (:1630): `ScriptsRoot`/`ScriptFolder` ⇒ same toggle; `ScriptFile`/`ScriptNotice` ⇒ no-op this task (T5/T6 wire them).
- `handle_single_click` (:1776): all four ⇒ select only this task.
- `row_is_expanded` (:1801): `ScriptsRoot` ⇒ `outer_expanded.contains(&OuterId::Scripts)`; `ScriptFolder { rel }` ⇒ contains `ScriptFolder(rel)`; under an active filter return `true` (Inner-row precedent); others ⇒ `false`.
- `row_in_active_scope` (:693): all four script variants ⇒ `false` (scripts are global; their own icons come in T5/T6, never ★/⊞/⇪).
- The `Render` impl's per-row visual match: `ScriptNotice` reuses the existing Notice color branch (muted; `danger` when the label starts with `"error:"`); `ScriptsRoot`/`ScriptFolder`/`ScriptFile` render as plain rows this task.

- [ ] **Step 6: Run tests + gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui schema_tree::` then the full 4-crate gate.
Expected: new tests PASS, every pre-existing flatten test still green (the `None` arg is a no-op by the dark-landing pin), zero warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/src/schema_tree.rs
git commit -m "feat: sidebar scripts section state + pure script_rows, dark (scripts T3)"
```

---

### Task 4 (T4): THE FLIP — settings row, scan dispatch, section live

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` — `render_settings_panel` (:1626)
- Modify: `crates/dbc-ui/src/schema_tree.rs` — `Render` impl flatten call (:1886), root-row `⟳` icon, notice/chevron event emission; `TreeEvent` (:90)
- Modify: `crates/dbc-ui/src/main.rs` — `on_tree_event` (the big `TreeEvent` match, grep `TreeEvent::OpenPreview`), new `dispatch_scripts_scan`/`pick_scripts_dir`/`clear_scripts_dir`, startup call site (AppView construction block, grep `discard_confirm: None`)

**Interfaces:**
- Consumes: T1 field, T2 `scan_scripts`, T3 state/methods.
- Produces: `TreeEvent::ScriptsRefresh`, `TreeEvent::OpenScriptsSettings`; `AppView::dispatch_scripts_scan(&mut self, cx: &mut Context<Self>)` (reused by T5 save-as and T6 mutations).

- [ ] **Step 1: TreeEvent variants + emission.** Extend `TreeEvent` (:90):

```rust
    /// Scripts library: rescan the library folder — the root row's ⟳, an
    /// error-notice retry click, or expanding the root while NotLoaded.
    ScriptsRefresh,
    /// Scripts library: the „není nastavena" notice was clicked — main.rs
    /// opens the Settings modal.
    OpenScriptsSettings,
```

In `handle_chevron`/`handle_double_click`'s `ScriptsRoot` arm (T3 made it toggle-only): after inserting `OuterId::Scripts` (i.e. on EXPAND), when `matches!(self.scripts, ScriptsListState::NotLoaded | ScriptsListState::Error(_)) && self.scripts_dir_set`, also `cx.emit(TreeEvent::ScriptsRefresh)` — the lazy-load-on-expand contract (`LoadDatabases` precedent). In `handle_single_click`'s `ScriptNotice` arm: `ScriptNoticeAction::Retry` ⇒ emit `ScriptsRefresh`; `OpenSettings` ⇒ emit `OpenScriptsSettings`; `None` ⇒ select only. In the `Render` impl, on the `ScriptsRoot` row append one inline icon div (★-precedent shape: `.id("scripts-refresh")`, `cx.stop_propagation()` in the click listener, then `cx.emit(TreeEvent::ScriptsRefresh)`, glyph `"⟳"`, `text_muted`).

- [ ] **Step 2: Flip the render call.** In the `Render` impl (:1886), change the `flatten_sidebar(…, None)` argument to:

```rust
            Some((&self.scripts, self.scripts_dir_set)),
```

- [ ] **Step 3: main.rs — scan dispatch + event arms.**

```rust
    /// Scripts library (design §1.2): one bounded background walk of
    /// `config.scripts_dir`; generation-guarded last-dispatched-wins.
    /// Callers: startup, folder (re)selection, ⟳/retry/expand events, and
    /// every successful in-app mutation (T5 save-as, T6 create/rename/
    /// delete). There is deliberately NO file watcher (design §1.2).
    pub(crate) fn dispatch_scripts_scan(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.config.scripts_dir.clone() else {
            self.tree.update(cx, |t, cx| t.clear_scripts(cx));
            return;
        };
        self.tree.update(cx, |t, cx| t.set_scripts_dir_present(true, cx));
        let generation = self.tree.update(cx, |t, cx| t.begin_scripts_scan(cx));
        let root = PathBuf::from(dir);
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { crate::scripts::scan_scripts(&root) }).await;
            let _ = this.update(cx, |view, cx| {
                view.tree.update(cx, |t, cx| t.apply_scripts_scan(generation, result, cx));
            });
        })
        .detach();
    }
```

In `on_tree_event`'s match add:

```rust
            TreeEvent::ScriptsRefresh => {
                self.dispatch_scripts_scan(cx);
            }
            TreeEvent::OpenScriptsSettings => {
                self.open_settings(cx);
            }
```

At the AppView construction site (grep `discard_confirm: None,` in the startup block), after the view is built and the initial schema fetch is kicked, add `view.dispatch_scripts_scan(cx);` in the same `update` the other startup dispatches use (relocate by the surrounding startup calls, e.g. where the initial `refresh_grouped_cache` runs).

- [ ] **Step 4: Settings rows.** In `render_settings_panel` (connections_ui.rs:1626), after the two theme radios and before the „Zavřít" button, add:

```rust
            .child(div().mt_2().text_color(cx.theme().text_muted).child("Složka skriptů"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().text_muted)
                    .child(self.config.scripts_dir.clone().unwrap_or_else(|| "nenastavena".to_string())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .id("settings-scripts-pick")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Vybrat složku…")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_scripts_dir(cx))),
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
                    ),
            )
```

And in main.rs (next to `dispatch_scripts_scan`; save-handling mirrors `set_theme`'s config-save error posture — relocate `set_theme` at :4421 and copy its save/error lines exactly):

```rust
    /// „Vybrat složku…" in Settings — directory picker; stores the
    /// ABSOLUTE path (a path, not a secret — config.toml is its home).
    pub(crate) fn pick_scripts_dir(&mut self, cx: &mut Context<Self>) {
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Vybrat".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                _ => return, // cancel/error: settings modal stays open, nothing changes
            };
            let _ = this.update(cx, |view, cx| {
                view.config.scripts_dir = Some(picked.display().to_string());
                view.save_config_or_status(cx); // the set_theme save+error lines, factored or repeated verbatim
                view.dispatch_scripts_scan(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// „Odebrat" — feature back to dormant. Deliberately does NOT touch a
    /// live editor binding (it holds an ABSOLUTE path; „Uložit" keeps
    /// working) — design §2.
    pub(crate) fn clear_scripts_dir(&mut self, cx: &mut Context<Self>) {
        self.config.scripts_dir = None;
        self.save_config_or_status(cx);
        self.tree.update(cx, |t, cx| t.clear_scripts(cx));
        cx.notify();
    }
```

If no `save_config_or_status` helper exists yet, add it as a 5-line wrapper over the exact save+error-status lines `set_theme` uses (one place, both callers) — do NOT invent new error copy.

- [ ] **Step 5: Manual smoke + gate**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui` — open ⚙ Nastavení → „Vybrat složku…" → pick a folder with a couple of `.sql` files and a subfolder → „Skripty" section appears, expands, ⟳ rescans, „Odebrat" reverts to the settings-pointer notice. Then the full 4-crate gate, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/main.rs crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: scripts section live — settings folder + background scan (scripts T4)"
```

---

### Task 5 (T5): Editor binding — open, Ctrl+S, caption, discard guard

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` — `actions!` (:67), `bind_keys` (:9157), root `.on_action` block (:8792), the editor `column` build (:8728), `PendingDiscard` (:988) + `on_discard_confirm_yes` (:6140) + `render_discard_confirm_overlay` (:7462), `on_tree_event`, AppView fields, palette dispatch (:4407 area) and palette history-entry click (:4334)
- Modify: `crates/dbc-ui/src/schema_tree.rs` — `TreeEvent::ScriptOpen` + double-click emit on `ScriptFile`
- Modify: `crates/dbc-ui/src/history_panel.rs` — the row-click `set_text` site (:256)
- Modify: `crates/dbc-ui/src/palette.rs` — `PaletteAction::SaveScript` + fixed row

**Interfaces:**
- Consumes: T2 `read_script`/`write_script`/`resolve_rel`/`SCRIPT_OPEN_CAP`, T4 `dispatch_scripts_scan`.
- Produces (used by T6): `ScriptBinding { path: PathBuf, saved_text: String }` on `AppView` (`script_binding: Option<ScriptBinding>`), `fn script_binding_dirty(&self, cx: &Context<Self>) -> bool`, `fn editor_load_guarded(&mut self, action: PendingScriptAction, cx)`, `enum PendingScriptAction { Open { rel: String }, Unbind, LoadText { sql: String } }`, `pub(crate) fn binding_display(path: &Path, scripts_dir: Option<&str>) -> String`.

- [ ] **Step 1: Failing test for the one pure helper.** In main.rs's test module (next to `list_sql_files_filters_and_orders_by_name`):

```rust
    #[test]
    fn binding_display_relativizes_inside_root_and_falls_back_to_name() {
        use std::path::Path;
        let p = Path::new("D:\\lib\\rep\\a.sql");
        assert_eq!(binding_display(p, Some("D:\\lib")), "rep/a.sql");
        assert_eq!(binding_display(p, Some("D:\\jinde")), "a.sql");
        assert_eq!(binding_display(p, None), "a.sql");
    }
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui binding_display` — expect compile FAIL, then implement:

```rust
/// Caption-strip label for a bound script: '/'-joined rel inside the
/// current library root, bare file name otherwise (the binding stores an
/// ABSOLUTE path precisely so it survives a root change — design §1.3).
pub(crate) fn binding_display(path: &Path, scripts_dir: Option<&str>) -> String {
    if let Some(root) = scripts_dir {
        if let Ok(rel) = path.strip_prefix(root) {
            let s: Vec<String> =
                rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if !s.is_empty() {
                return s.join("/");
            }
        }
    }
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}
```

Re-run: PASS.

- [ ] **Step 2: Binding state + guard plumbing.** Add to `AppView` fields (near `discard_confirm` :1197): `script_binding: Option<ScriptBinding>` (+ `script_binding: None,` at the construction site). Types next to `PendingDiscard`:

```rust
/// Scripts library (design §1.3): the single global editor bound to one
/// on-disk `.sql` file. `path` is ABSOLUTE; dirty = editor text !=
/// `saved_text` (bounded by SCRIPT_OPEN_CAP, design §7.6).
struct ScriptBinding {
    path: PathBuf,
    saved_text: String,
}

/// The action a script dirty-guard parks (design §5.5). Deliberately
/// NEVER guards unbound ad-hoc editor text — identical exposure to
/// pre-phase behaviour.
enum PendingScriptAction {
    Open { rel: String },
    Unbind,
    LoadText { sql: String },
}
```

Extend `PendingDiscard` (:988) with `Script(PendingScriptAction)`, its `on_discard_confirm_yes` (:6140) arm:

```rust
            PendingDiscard::Script(action) => {
                self.perform_script_action(action, cx);
            }
```

(`on_discard_confirm_no` needs no new code — take-and-drop already aborts.) In `render_discard_confirm_overlay` (:7462) branch the message line:

```rust
        let msg = match &dc.action {
            PendingDiscard::Script(_) => {
                let name = self
                    .script_binding
                    .as_ref()
                    .and_then(|b| b.path.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("Neuložené změny skriptu {name} — zahodit?")
            }
            _ => format!("Neuložené změny ({n}) — zahodit?"),
        };
```

and use `msg` where the literal `format!("Neuložené změny ({n}) — zahodit?")` sits today (:7479). Then the guard + performer:

```rust
    fn script_binding_dirty(&self, cx: &Context<Self>) -> bool {
        match &self.script_binding {
            Some(b) => self.sql.read(cx).text() != b.saved_text,
            None => false,
        }
    }

    /// Every editor-text replacement routes here (design §5.5): dirty
    /// bound script ⇒ confirm; else perform immediately.
    fn editor_load_guarded(&mut self, action: PendingScriptAction, cx: &mut Context<Self>) {
        if self.script_binding_dirty(cx) {
            self.discard_confirm =
                Some(DiscardConfirmState { change_count: 0, action: PendingDiscard::Script(action) });
            self.modal_needs_focus = true;
            cx.notify();
            return;
        }
        self.perform_script_action(action, cx);
    }

    fn perform_script_action(&mut self, action: PendingScriptAction, cx: &mut Context<Self>) {
        match action {
            PendingScriptAction::Unbind => {
                self.script_binding = None;
                cx.notify();
            }
            PendingScriptAction::LoadText { sql } => {
                self.script_binding = None;
                self.sql.update(cx, |s, cx| s.set_text(&sql, cx));
                cx.notify();
            }
            PendingScriptAction::Open { rel } => {
                let Some(root) = self.config.scripts_dir.clone() else {
                    self.status = "error: nastavte složku skriptů v Nastavení".to_string();
                    cx.notify();
                    return;
                };
                let path = match crate::scripts::resolve_rel(&PathBuf::from(root), &rel) {
                    Ok(p) => p,
                    Err(e) => {
                        self.status = format!("error: {e}");
                        cx.notify();
                        return;
                    }
                };
                let read_path = path.clone();
                cx.spawn(async move |this, cx| {
                    let res = cx
                        .background_spawn(async move { crate::scripts::read_script(&read_path) })
                        .await;
                    let _ = this.update(cx, |view, cx| match res {
                        Ok(text) => {
                            view.sql.update(cx, |s, cx| s.set_text(&text, cx));
                            view.script_binding =
                                Some(ScriptBinding { path, saved_text: text });
                            view.status = String::new();
                            cx.notify();
                        }
                        Err(e) => {
                            view.status = format!("error: {e}");
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
    }
```

- [ ] **Step 3: Tree open path.** schema_tree.rs: add `TreeEvent` variant

```rust
    /// Scripts library: double-click on a `.sql` row — main.rs loads the
    /// file into the (guarded) global editor. NEVER executes anything.
    ScriptOpen { rel: String },
```

and emit it from `handle_double_click`'s `ScriptFile { rel }` arm. main.rs `on_tree_event`:

```rust
            TreeEvent::ScriptOpen { rel } => {
                self.editor_load_guarded(PendingScriptAction::Open { rel }, cx);
            }
```

- [ ] **Step 4: Ctrl+S.** `actions!` (:67) gains `SaveScript`; `bind_keys` (:9158-9166) gains `KeyBinding::new("ctrl-s", SaveScript, None)`; the root div (:8792) gains `.on_action(cx.listener(Self::on_save_script))`. Handler + flows:

```rust
    /// Ctrl+S / caption „Uložit": bound ⇒ atomic save; unbound ⇒ save-as
    /// into the library (design §5.2/§5.4). Inert under any open overlay
    /// (a modal's text field must not trigger file writes).
    fn on_save_script(&mut self, _: &SaveScript, _window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.script_binding.is_some() {
            self.save_bound_script(cx);
        } else {
            self.save_script_as(cx);
        }
    }

    fn save_bound_script(&mut self, cx: &mut Context<Self>) {
        let Some(b) = &self.script_binding else { return };
        let path = b.path.clone();
        let guard_path = path.clone();
        let text = self.sql.read(cx).text();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_spawn(async move {
                    crate::scripts::write_script(&path, &text).map(|_| text)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match res {
                    // Guard: only stamp saved_text if the binding still
                    // targets the same file (an open raced the save).
                    Ok(text) => {
                        if let Some(b) = &mut view.script_binding {
                            if b.path == guard_path {
                                b.saved_text = text;
                            }
                        }
                        view.status = format!("skript uložen: {name}");
                    }
                    Err(e) => view.status = format!("error: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_script_as(&mut self, cx: &mut Context<Self>) {
        let text = self.sql.read(cx).text();
        if text.trim().is_empty() {
            self.status = "editor je prázdný".to_string();
            cx.notify();
            return;
        }
        let Some(root) = self.config.scripts_dir.clone() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let root = PathBuf::from(root);
        let dialog = cx.prompt_for_new_path(&root, Some("dotaz.sql"));
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(p))) => p,
                _ => return,
            };
            // Client-side .sql append (no dialog filter API at the pinned
            // rev — G12 precedent).
            let picked = if picked
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
            {
                picked
            } else {
                let mut os = picked.into_os_string();
                os.push(".sql");
                PathBuf::from(os)
            };
            let write_path = picked.clone();
            let write_text = text.clone();
            let res = cx
                .background_spawn(async move {
                    crate::scripts::write_script(&write_path, &write_text)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match res {
                    Ok(()) => {
                        let name = picked
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        view.script_binding =
                            Some(ScriptBinding { path: picked.clone(), saved_text: text });
                        view.status = format!("skript uložen: {name}");
                        if picked.starts_with(&root) {
                            view.dispatch_scripts_scan(cx);
                        }
                    }
                    Err(e) => view.status = format!("error: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
```

- [ ] **Step 5: Caption strip.** In `render` where `column` is built (:8728), insert BEFORE the editor child:

```rust
        let caption = self.script_binding.as_ref().map(|b| {
            let dirty = self.script_binding_dirty(cx);
            let label = format!(
                "Skript: {}{}",
                binding_display(&b.path, self.config.scripts_dir.as_deref()),
                if dirty { " •" } else { "" }
            );
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(24.))
                .px_2()
                .bg(theme.bg_app)
                .text_sm()
                .child(div().text_color(theme.text_muted).child(label))
                .child(
                    div()
                        .id("script-save")
                        .px_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .text_color(if dirty { theme.text_primary } else { theme.text_disabled })
                        .child("Uložit")
                        .on_click(cx.listener(|view, _, _, cx| view.save_bound_script(cx))),
                )
                .child(
                    div()
                        .id("script-unbind")
                        .px_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .child("Zavřít")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.editor_load_guarded(PendingScriptAction::Unbind, cx)
                        })),
                )
        });
        let mut column = div().flex().flex_col().flex_1().min_w_0();
        if let Some(strip) = caption {
            column = column.child(strip);
        }
        column = column.child(/* the EXISTING editor div (:8733-8751), unchanged */);
```

(Restructure the existing `let mut column = div()...child(editor)` chain into this shape; the editor div itself moves verbatim.)

- [ ] **Step 6: Reroute the two existing set_text sites through the guard.**
  - `history_panel.rs:256`: replace the direct `view.sql.update(cx, |s, cx| s.set_text(&sql, cx))` (relocate by grep `set_text` in that file) with `view.editor_load_guarded(PendingScriptAction::LoadText { sql }, cx);` — make `PendingScriptAction` `pub(crate)` for this.
  - `main.rs:4334` (palette history click): same replacement.

- [ ] **Step 7: Palette entry.** palette.rs: add variant + row after `RunSqlFolder` (:218):

```rust
    /// Scripts library: Ctrl+S equivalent — save the bound script, or
    /// save-as into the library (`AppView::on_save_script` flows).
    SaveScript,
```

```rust
        ("Uložit skript".to_string(), PaletteAction::SaveScript),
```

main.rs palette dispatch (:4407 area): `PaletteAction::SaveScript => { self.on_save_script(&SaveScript, window, cx); }` — match the surrounding arms' exact call shape for window availability; if the dispatch site has no `window`, add a `save_script_dispatch(&mut self, cx)` that both the action handler and this arm call (the handler body above never uses `window`).

- [ ] **Step 8: Manual smoke + gate**

`cargo run -p dbc-ui`: double-click a script → editor fills, caption shows „Skript: rep/a.sql"; type → „ •" appears; Ctrl+S → „skript uložen: a.sql", • clears; double-click another script while dirty → „Neuložené změny skriptu a.sql — zahodit?"; „Zrušit" keeps text; history row click while dirty → same guard; with no binding Ctrl+S on non-empty editor → save-as dialog into the library and the tree shows the new file. Then the full 4-crate gate, zero warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/dbc-ui/src/{main.rs,schema_tree.rs,history_panel.rs,palette.rs}
git commit -m "feat: script editor binding — open, Ctrl+S, dirty guard (scripts T5)"
```

---

### Task 6 (T6): Mutations (new/rename/delete) + run-from-tree

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs` — `TreeEvent` + row icons (`+`/`✎`/`✕`/`▶`)
- Modify: `crates/dbc-ui/src/connections_ui.rs` — `ModalState` (:1294 area), `modal_confirm_kind` (:1340), two render panels
- Modify: `crates/dbc-ui/src/main.rs` — `on_tree_event` arms, modal open/confirm fns, Esc-closable match (grep `ModalState::Settings => true`), Enter-confirm dispatch (grep `ModalConfirmKind::CloseSettings` usage), factor `open_script_run_modal` out of `start_script_pick` (:2701-2744)

**Interfaces:**
- Consumes: T2 fs ops, T4 `dispatch_scripts_scan`, T5 binding.
- Produces: `TreeEvent::{ScriptCreate { parent_rel: String }, ScriptRename { rel: String, is_dir: bool }, ScriptDelete { rel: String, is_dir: bool }, ScriptRunFile { rel: String }}`; `ModalState::{ScriptName { .. }, ScriptDeleteConfirm { .. }}`; `ModalConfirmKind::ScriptName`; `AppView::open_script_run_modal(..)`.

- [ ] **Step 1: Failing test for the confirm-policy table.** In connections_ui.rs's test module (next to any existing `modal_confirm_kind` test; the `ScriptDeleteConfirm` variant is entity-free, so this test needs no GPUI context — the `ScriptName` side of the policy is pinned by `modal_confirm_kind`'s deliberate no-`_` exhaustiveness instead, see the comment at :1350):

```rust
    #[test]
    fn script_delete_confirm_enter_is_ignore() {
        // §3-novela substance: the button is the last gate before an
        // IRREVERSIBLE action (filesystem, not database — the rule's
        // point is irreversibility). Enter must be a handled no-op.
        let state = ModalState::ScriptDeleteConfirm {
            rel: "rep/a.sql".to_string(),
            is_dir: false,
            bound_dirty: false,
        };
        assert!(matches!(modal_confirm_kind(&state), ModalConfirmKind::Ignore));
    }
```

- [ ] **Step 2: ModalState + policy + Esc.** connections_ui.rs, after `ChartPicker`:

```rust
    /// Scripts library (design §4): create (script/folder radio) or
    /// rename. `target_rel: Some(..)` = rename mode; `parent_rel` is the
    /// creation parent ('' = root). `error` renders inline in `danger`.
    ScriptName {
        parent_rel: String,
        target_rel: Option<String>,
        is_dir: bool,
        field: gpui::Entity<TextField>,
        error: Option<String>,
    },
    /// Scripts library (design §4): irreversible-delete confirm.
    /// `bound_dirty` adds the „neuložené změny" second line when the
    /// target is the dirty-bound file (one modal, both facts — resolved
    /// deviation 4).
    ScriptDeleteConfirm { rel: String, is_dir: bool, bound_dirty: bool },
```

`modal_confirm_kind`: `ModalState::ScriptName { .. } => ModalConfirmKind::ScriptName` (new variant, dispatching to `AppView::confirm_script_name` at the same site that dispatches the other confirm kinds); move `ScriptDeleteConfirm` into the explicit `Ignore` group (:1353-1357). main.rs Esc-closable match: both variants ⇒ `true`.

- [ ] **Step 3: Render panels** (connections_ui.rs, next to `render_settings_panel`, same visual grammar — 360 px panel, title, rows, button row):

```rust
    fn render_script_name_panel(
        &mut self,
        parent_rel: &str,
        target_rel: Option<&str>,
        is_dir: bool,
        field: &gpui::Entity<TextField>,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_rename = target_rel.is_some();
        let title = if is_rename {
            "Přejmenovat".to_string()
        } else if parent_rel.is_empty() {
            "Nová položka".to_string()
        } else {
            format!("Nová položka — {parent_rel}/")
        };
        let kind_radio = |id: &'static str, label: &'static str, dir: bool, current: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .bg(if dir == current { cx.theme().bg_selected } else { cx.theme().bg_hover })
                .child(if dir == current { "●" } else { "○" })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_script_name_kind(dir, cx)))
        };
        let mut panel = div()
            .id("script-name-panel")
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
            .child(div().text_size(px(16.)).child(title))
            .child(field.clone());
        if !is_rename {
            panel = panel
                .child(kind_radio("script-kind-file", "Skript (.sql)", false, is_dir, cx))
                .child(kind_radio("script-kind-dir", "Složka", true, is_dir, cx));
        }
        if let Some(e) = error {
            panel = panel.child(div().text_color(cx.theme().danger).child(e.to_string()));
        }
        panel
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .id("script-name-cancel")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Zrušit")
                            .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
                    )
                    .child(
                        div()
                            .id("script-name-save")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Uložit")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_script_name(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_script_delete_panel(
        &mut self,
        rel: &str,
        is_dir: bool,
        bound_dirty: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let what = if is_dir { "složku" } else { "skript" };
        let mut panel = div()
            .id("script-delete-panel")
            .w(px(420.))
            .bg(cx.theme().bg_panel)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .text_color(cx.theme().text_primary)
            .child(format!("Smazat {what} {name}? Akce je nevratná (maže se z disku, ne do koše)."));
        if bound_dirty {
            panel = panel.child(
                div().text_color(cx.theme().warn).child("Skript má neuložené změny v editoru."),
            );
        }
        panel
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .id("script-delete-cancel")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .cursor_pointer()
                            .child("Zrušit")
                            .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
                    )
                    .child(
                        div()
                            .id("script-delete-yes")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(cx.theme().bg_hover)
                            .text_color(cx.theme().danger)
                            .cursor_pointer()
                            .child("Smazat")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_script_delete(cx))),
                    ),
            )
            .into_any_element()
    }
```

Wire both into the modal render dispatch (grep `render_settings_panel(` call site — the `ModalState` render match) and add `set_script_name_kind`:

```rust
    pub(crate) fn set_script_name_kind(&mut self, is_dir: bool, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ScriptName { is_dir: d, .. }) = &mut self.modal {
            *d = is_dir;
            cx.notify();
        }
    }
```

(If the render helpers live in connections_ui.rs but the state mutators live on `AppView` in main.rs — follow the file split the existing Settings/ConnectionDialog pair uses: renderers in connections_ui.rs `impl AppView`, mutators wherever `set_theme` lives.)

- [ ] **Step 4: Open/confirm handlers** (main.rs):

```rust
    fn open_script_name_modal(
        &mut self,
        parent_rel: String,
        target_rel: Option<String>,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        let field = cx.new(|cx| connections_ui::TextField::new(cx, "název", false));
        if let Some(rel) = &target_rel {
            let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
            field.update(cx, |f, cx| f.set_text(&name, cx));
        }
        self.modal = Some(connections_ui::ModalState::ScriptName {
            parent_rel,
            target_rel,
            is_dir,
            field,
            error: None,
        });
        self.modal_needs_focus = true;
        let _ = window; // focus handling follows the ConnectionDialog pattern; drop if unused
        cx.notify();
    }

    pub(crate) fn confirm_script_name(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptName { parent_rel, target_rel, is_dir, field, .. }) =
            &self.modal
        else {
            return;
        };
        let (parent_rel, target_rel, is_dir) = (parent_rel.clone(), target_rel.clone(), *is_dir);
        let name = field.read(cx).text();
        let Some(root) = self.config.scripts_dir.clone() else {
            self.modal = None;
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let root = PathBuf::from(root);
        let op_root = root.clone();
        cx.spawn(async move |this, cx| {
            let res: Result<(Option<String>, String), String> = cx
                .background_spawn(async move {
                    match &target_rel {
                        Some(rel) => crate::scripts::rename_entry(&op_root, rel, &name, is_dir)
                            .map(|new_rel| (Some(rel.clone()), new_rel)),
                        None => {
                            if is_dir {
                                crate::scripts::create_folder(&op_root, &parent_rel, &name)
                                    .map(|r| (None, r))
                            } else {
                                crate::scripts::create_script(&op_root, &parent_rel, &name)
                                    .map(|r| (None, r))
                            }
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match res {
                    Ok((old_rel, new_rel)) => {
                        // Rename of the BOUND file: fix the binding path.
                        if let (Some(old_rel), Some(b)) = (&old_rel, &mut view.script_binding) {
                            if let (Ok(oldp), Ok(newp)) = (
                                crate::scripts::resolve_rel(&root, old_rel),
                                crate::scripts::resolve_rel(&root, &new_rel),
                            ) {
                                if b.path == oldp {
                                    b.path = newp;
                                }
                            }
                        }
                        let name = new_rel.rsplit('/').next().unwrap_or(&new_rel).to_string();
                        view.status = if old_rel.is_some() {
                            format!("přejmenováno: {name}")
                        } else {
                            format!("skript vytvořen: {name}")
                        };
                        if matches!(view.modal, Some(connections_ui::ModalState::ScriptName { .. })) {
                            view.modal = None;
                        }
                        view.dispatch_scripts_scan(cx);
                    }
                    Err(e) => {
                        if let Some(connections_ui::ModalState::ScriptName { error, .. }) =
                            &mut view.modal
                        {
                            *error = Some(e);
                        } else {
                            view.status = format!("error: {e}");
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_script_delete_confirm(&mut self, rel: String, is_dir: bool, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        let bound_dirty = match (&self.script_binding, self.config.scripts_dir.as_deref()) {
            (Some(b), Some(root)) => {
                crate::scripts::resolve_rel(Path::new(root), &rel).is_ok_and(|p| p == b.path)
                    && self.script_binding_dirty(cx)
            }
            _ => false,
        };
        self.modal =
            Some(connections_ui::ModalState::ScriptDeleteConfirm { rel, is_dir, bound_dirty });
        self.modal_needs_focus = true;
        cx.notify();
    }

    pub(crate) fn confirm_script_delete(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptDeleteConfirm { rel, is_dir, .. }) = &self.modal
        else {
            return;
        };
        let (rel, is_dir) = (rel.clone(), *is_dir);
        let Some(root) = self.config.scripts_dir.clone() else {
            self.modal = None;
            cx.notify();
            return;
        };
        let root = PathBuf::from(root);
        let op_root = root.clone();
        let op_rel = rel.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_spawn(async move { crate::scripts::delete_entry(&op_root, &op_rel, is_dir) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if matches!(view.modal, Some(connections_ui::ModalState::ScriptDeleteConfirm { .. })) {
                    view.modal = None;
                }
                match res {
                    Ok(()) => {
                        if let (Some(b), Ok(p)) =
                            (&view.script_binding, crate::scripts::resolve_rel(&root, &rel))
                        {
                            if b.path == p {
                                view.script_binding = None; // the confirmed delete IS the discard
                            }
                        }
                        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                        view.status = format!("smazáno: {name}");
                        view.dispatch_scripts_scan(cx);
                    }
                    Err(e) => view.status = format!("error: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
```

- [ ] **Step 5: TreeEvents + icons.** schema_tree.rs `TreeEvent` gains:

```rust
    /// Scripts library: „+" on the root/folder row — main.rs opens the
    /// name modal targeting this parent ('' = root).
    ScriptCreate { parent_rel: String },
    /// Scripts library: „✎" — main.rs opens the name modal in rename mode.
    ScriptRename { rel: String, is_dir: bool },
    /// Scripts library: „✕" — main.rs opens the irreversible-delete confirm.
    ScriptDelete { rel: String, is_dir: bool },
    /// Scripts library: „▶" on a `.sql` row — main.rs routes into the
    /// UNCHANGED G12 confirm flow (never a direct execution).
    ScriptRunFile { rel: String },
```

In the `Render` icon block (★/⊞/⇪ shape, :2039-2116): `ScriptsRoot` row appends `+` (emit `ScriptCreate { parent_rel: "" }`) before the T4 `⟳`; `ScriptFolder { rel }` appends `+` (`ScriptCreate { parent_rel: rel }`), `✎` (`ScriptRename`), `✕` (`ScriptDelete`); `ScriptFile { rel }` appends `▶` (`ScriptRunFile`), `✎`, `✕`. Every icon div: unique `.id(("script-ic-…", ix))`, `cx.stop_propagation()` first in the listener, `text_muted` color (the `✕` uses `danger` on the same dim-idle convention the tab-close button uses).

main.rs `on_tree_event` arms:

```rust
            TreeEvent::ScriptCreate { parent_rel } => {
                self.open_script_name_modal(parent_rel, None, false, window, cx);
            }
            TreeEvent::ScriptRename { rel, is_dir } => {
                self.open_script_name_modal(String::new(), Some(rel), is_dir, window, cx);
            }
            TreeEvent::ScriptDelete { rel, is_dir } => {
                self.open_script_delete_confirm(rel, is_dir, cx);
            }
            TreeEvent::ScriptRunFile { rel } => {
                self.start_script_run_from_library(rel, cx);
            }
```

(If `on_tree_event` has no `window` parameter, follow the existing pattern for modal-opening arms — the ConnectionDialog open goes through `modal_needs_focus`, so the `window` argument can be dropped from `open_script_name_modal` entirely; keep the signature window-free in that case.)

- [ ] **Step 6: Run-from-tree = factored G12 continuation.** In `start_script_pick` (:2701-2744), extract the `Ok((source_label, files, file_counts))` arm's body (modal-race check, identity re-check, `ModalState::ScriptRun` construction, `modal_needs_focus`) into:

```rust
    /// The SHARED confirm-modal continuation of every script-run entry
    /// point (G12 picker AND the scripts library ▶) — one policy site,
    /// design §6. Everything downstream (`confirm_script_run`'s re-checks,
    /// `script_run_dispatch_allowed`, runner gates) is untouched.
    #[allow(clippy::too_many_arguments)] // mirrors the modal's own field count
    fn open_script_run_modal(
        &mut self,
        source_label: String,
        files: Vec<PathBuf>,
        file_counts: Vec<usize>,
        conn_label: String,
        read_only: bool,
        timeout_secs: Option<u64>,
        conn_identity: String,
        cx: &mut Context<Self>,
    ) { /* the moved arm body, verbatim */ }
```

then the new entry point:

```rust
    /// ▶ on a library row: same gates as `start_script_pick`, with the
    /// file picker replaced by the known library path.
    fn start_script_run_from_library(&mut self, rel: String, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.cancel.is_some() {
            return;
        }
        let Some((read_only, timeout_secs, engine, _spec)) = self.resolve_spec_for_explain(cx) else {
            return;
        };
        let Some(dialect) = dialect_for_engine(engine) else {
            self.status = "error: skripty nejsou podporovány pro tento engine".to_string();
            cx.notify();
            return;
        };
        let conn_label = self.current_connection_label();
        let conn_identity = self.current_conn_identity();
        let Some(root) = self.config.scripts_dir.clone() else { return };
        let path = match crate::scripts::resolve_rel(&PathBuf::from(root), &rel) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("error: {e}");
                cx.notify();
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let result: Result<(String, Vec<PathBuf>, Vec<usize>), String> = cx
                .background_spawn(async move {
                    if !path.is_file() {
                        return Err("soubor už neexistuje — obnovte strom (⟳)".to_string());
                    }
                    let count = count_statements_in_file(&path, dialect)?;
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    Ok((name, vec![path], vec![count]))
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok((label, files, counts)) => view.open_script_run_modal(
                    label,
                    files,
                    counts,
                    conn_label,
                    read_only,
                    timeout_secs,
                    conn_identity,
                    cx,
                ),
                Err(e) => {
                    view.status = format!("error: {e}");
                    cx.notify();
                }
            });
        })
        .detach();
    }
```

- [ ] **Step 7: Manual smoke + gate**

`cargo run -p dbc-ui`: `+` on root → „Nová položka" → radio Složka → creates folder; `+` on the folder → creates script inside; `✎` rename (collision shows „název už existuje" inline, Enter confirms, Esc closes); `✕` on non-empty folder → „složka není prázdná…"; `✕` on file → confirm modal, Enter does NOTHING (Ignore), „Smazat" deletes + tree refreshes; `▶` on a file → the familiar G12 confirm modal with per-file counts, „Spustit" runs it into the Skript progress tab; deleting the dirty-bound file shows the „neuložené změny" line. Full 4-crate gate, zero warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/dbc-ui/src/{main.rs,schema_tree.rs,connections_ui.rs}
git commit -m "feat: scripts mutations + run-from-tree via G12 confirm (scripts T6)"
```

---

### Task 7 (T7): Docs as-built, version 0.22.0, final gates

**Files:**
- Modify: root `Cargo.toml` (`[workspace.package] version`, :7)
- Modify: `docs/superpowers/specs/drafts/scripts-library-design.md` (append „As-built" footer)
- Modify: project memory file `db-client-project.md` (auto-memory dir — one line: scripts library shipped, v0.22.0)

- [ ] **Step 1: Bump.** `version = "0.21.0"` → `version = "0.22.0"` (verify 0.22.0 is still free on main first — phase convention).

- [ ] **Step 2: As-built footer.** Append to the design doc: resolved deviations that actually materialized (start from this plan's four + anything discovered), disclosed limits (2000 entries, depth 12, 1 MiB editor cap, `.sql`-only, symlinks skipped, no watcher, last-writer-wins saves, no exit guard, empty-folder-only delete), and behavioral release notes (nová sekce „Skripty", Ctrl+S, ▶ přes stávající potvrzovací dialog, git zůstává externí).

- [ ] **Step 3: Final gates** (verification-before-completion — run ALL, paste outputs):

1. `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` — green, zero warnings.
2. `%USERPROFILE%\.cargo\bin\cargo.exe build --workspace` and `... build --workspace --release` — zero warnings in both.
3. `cargo run -p dbc-ui` manual walkthrough: settings folder pick → tree → open → edit → Ctrl+S → rename → run ▶ → delete; window title reads `dbc v0.22.0`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock docs/superpowers/specs/drafts/scripts-library-design.md
git commit -m "chore: v0.22.0 — scripts library (scripts T7)"
```

Then follow superpowers:finishing-a-development-branch (merge commit shape: `Merge feature/scripts-library: Bruno-style scripts library (v0.22.0)`).

---

## Open questions deliberately DECIDED

1. **Watcher vs manual refresh** → manual ⟳ + rescan-after-every-in-app-mutation + scan-on-expand/startup; no `notify` (design §1.2 carries the cost/benefit).
2. **Per-tab editors** → global-editor binding (design §1.3; per-tab editors are the g6-editor-pro rework, not this phase).
3. **`.sql`-only tree** → yes (design §1.5).
4. **Folder deletes** → empty-only; git restores files, we can't (design §7.9).
5. **Run uses disk content even when the binding is dirty** → yes, disclosed by the • (design §1.3; auto-save-before-run would be a silent write).
6. **Enter policy** → ScriptName confirms, ScriptDeleteConfirm ignores (policy table clauses applied in T6 Step 2).
