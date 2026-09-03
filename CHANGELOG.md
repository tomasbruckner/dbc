# Changelog

All notable changes to dbc. The format follows [Keep a Changelog](https://keepachangelog.com/);
entries are the `feat:`/`fix:` commit titles that landed in each version, so
`git log v0.30.0..v0.31.0` tells the full story behind any line here.

## [Unreleased]

## [0.32.0] - 2026-09-03

The first version shipped as a GitHub Release.

### Added / changed

- a release build (static CRT, no VC++ redistributable needed), an exe icon and version info, and a README for colleagues
- what a public repository needs: LICENSE (MIT), SECURITY, CONTRIBUTING, CHANGELOG, CODEOWNERS, CI and a tag-driven release workflow

### Fixed

- CI failed on a CRLF checkout

## [0.31.0] - 2026-09-02

### Added / changed

- the vault opens in a blink
- the editor takes the whole column until a result arrives
- the history is a tab, not a panel

## [0.30.0] - 2026-09-02

### Fixed

- the editor drew un-highlighted text black on the dark input

## [0.29.0] - 2026-09-02

### Added / changed

- the server version is remembered between runs
- the sidebar shows a connection's databases before it can reach it
- choose which databases and schemas the sidebar shows
- query tabs, a Ctrl+D database picker, scrollbars, and quoted Postgres names

### Fixed

- MSSQL refused a host it could have read
- five things the dialogs got wrong, and the components that stop them
- text that ran outside its box, and a grid whose header did not line up
- collapsing a folder left the folders inside it on screen
- Ctrl+Shift+E did nothing after T-SQL's TOP

## [0.28.0] - 2026-09-01

### Added / changed

- settings can move to another computer as one file
- the connection row says which server version it is

### Fixed

- MSSQL writes were cancelled before the server finished them
- the live MSSQL suite leaked a 1.25 GB container per run
- modal dialogs opened with nothing focused

## [0.27.0] - 2026-08-31

### Added / changed

- CLI runs go into history, in their own section

### Fixed

- a long error in history painted over the row below it
- the history splitter never resized anything
- the history row promised an opening it did not have

## [0.26.0] - 2026-08-31

### Added / changed

- dbc — a command line over the connections saved in the app

## [0.25.0] - 2026-08-31

### Added / changed

- text selection in read-only text tabs
- expand SELECT * to columns, and FK-driven JOIN suggestions
- reopen in the state the app was closed in
- log session save and restore
- restore the expanded state INSIDE a database too
- delete a saved connection; make TextField selection visible
- warm the schema cache for idle databases in the background

### Fixed

- release-only test failures, clippy errors, single owner for .tmp
- the JOIN suggestion popup was cutting its labels off
- the session was never saved — the quit hook had nothing to run on
- the session restore was refused by the startup vault prompt
- Kind-mode sections were pruned as stale on every refresh

## [0.24.0] - 2026-08-31

### Added / changed

- shortcut cheat sheet, focus-aware hint strip, pane focus keys
- hamburger app menu, About panel; fix the clipped F1 sheet
- draw our own title bar, system decoration off
- manage connection folders — create, rename, delete, drag between

## [0.23.0] - 2026-08-30

### Added / changed

- recovery adopt now discloses the vault and the git warning (final-review MINOR-1)
- compiler-enforced permit for replace_buffer (re-verify: the wrong decline)
- relocate the profile dir via DBC_DATA_DIR, on ONE rail
- engine picker is a segmented row, not a cycle button
- draggable sidebar splitter, width persisted across restarts
- schema tree can group by object kind instead of by schema
- SQL formatter (Ctrl+Shift+F) and per-dialect syntax highlighting
- Windows text-editing shortcuts in the SQL editor
- right-click context menus in the schema tree
- diagnostic log with a closed event vocabulary
- editor focus on click, async unlock, schema cache, panel + bar rework

### Fixed

- re-ask the binding in finish_script_delete (final-review MAJOR-1)
- fold the root comparison in script_open_abort_reason (final-review NIT-3)
- save_script_as re-checks the captured buffer too (final-review NIT-1)
- a blocked start's paths are always absolute (final-review MINOR-2)
- compiler-enforced Ctrl+S guard witness for save_script (final-review MAJOR-2, part 1)
- compiler-enforced config guard + a real source scanner (final-review MAJOR-2, part 2)
- the save permission is a scope, not a carriable value (re-verify FAIL-2)
- a landed delete supersedes in-flight opens unconditionally (re-verify MINOR-A)
- code_lines parses byte and C string prefixes (re-verify FAIL-3)
- audits match the NAME and see the whole tree (re-verify FAIL-1, FAIL-4)
- forbid unsafe, path-bind the config guard, split unreadable from corrupt (re-verify MINOR-B, MINOR-C, NIT-1, NIT-2)
- four scanner predicates that a spelling walked past (re-verify FAIL-6/7/8/9)
- re-verify NITs + correct two false claims in the docs
- two audit predicates that failed open (re-verify FAIL-14, FAIL-13)
- accept_completion cannot clear the buffer (re-verify: the second mutator)
- ask the COMPILER, not the source, which files were read (option (a))
- autocomplete after 'schema.' offered nothing (user report: dbo.)
- editor is focused at startup; Ctrl+Space says why it offered nothing
- ER diagram from a database row drew nothing; context menu was clipped
- five things the log got wrong, plus Ctrl+P for the palette
- schema cache I/O was on the UI thread

## [0.22.0] - 2026-08-26

### Added / changed

- AppConfig.scripts_dir additive field (scripts T1)
- scripts.rs — scan/validate/fs ops with safety rails (scripts T2)
- shared fs rails in dbc-state::fsutil, scripts.rs delegates (workspace T1)
- dbc-state::workspace — pointer, marker, classify, resolve, crash-safe init (workspace T2)
- schema_tree scripts section — types, state, pure emission (dark) (workspace T3)
- startup workspace resolution + blocking WorkspaceMissing modal + apply_context swap (workspace T4)
- Settings workspace block + init/adopt/leave confirm modals + gated context swap (workspace T5)
- dbc-mcp resolves the workspace pointer, fails loudly when broken (workspace T6)
- scripts section live over effective_scripts_root (both arms), settings blocks (workspace T7)
- script editor binding — caption strip, Ctrl+S save/save-as, dirty guard (workspace T8)
- script create/rename/delete modals + library run via factored open_script_run_modal (workspace T9)

### Fixed

- Unicode-aware collision probe + root-mutation rail (scripts T2 review)
- pointer existence probe can no longer fall back to profile silently
- drop the unreadable ScriptsListState::Loading generation; O(n) folder prune
- T4 review — guard the recovery pick, make blocked_paths structurally safe (workspace T4)
- §W4 choice buttons activate on Enter, not only Space (workspace T4 carry-forward)
- T5 review — guard the pick continuation, deliver the refusal readably (workspace T5)
- T6 review — pin the pointer copy, stop a typo masking the diagnosis, one-line the reason (workspace T6)
- T7 review — guard every config.toml writer, deliver the scripts-pick refusal (workspace T7)
- T8 review — guard the buffer not just the binding, serialize saves, make the clobber audit sound (workspace T8)
- T8 re-verify MAJOR-3 — scan the whole crate, sanction by exact fn name (workspace T8)
- T8 re-verify MAJOR — a swap and a root change both supersede an in-flight open (workspace T8)
- T9 review MAJOR-1/MAJOR-2 - guard Ctrl+S behind dialogs, audit the writer crate-wide (workspace T9)
- T9 review MINOR-1 - fold case Unicode-aware in every binding comparison (workspace T9)
- T9 review MINOR-2/3/4 + NIT-1/2 - symmetric identity re-check, run-confirm disclosure, pinned copy (workspace T9)
- T10 carry-forwards 1/2/3/5/6/7/8 — compiler-backed config guard, one fold, one reason collapse (workspace T10)
- T9 re-verify FAIL-1 — re-ask script_save_allowed after the save-as picker, audit the writer's callers (workspace T10)

## [0.21.0] and earlier - up to 2026-08-25

Phases G1–G16 of the original build-out: connections and the vault, the
editor with tree-sitter highlighting and autocomplete, the result grid,
history, plans, ER diagrams, schema/data compare, monitor, admin panel,
backups, CSV import, DuckDB. Written before this changelog existed;
`git log` is the record.
