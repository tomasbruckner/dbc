# G11 — Backup & Restore: Design Pass

Date: 2026-08-23
Status: designed autonomously per the G5-style standing mandate; decisions
recorded here for later user review.
Scope: spec row G11 (`docs/superpowers/specs/2026-08-22-gui-target-design.md`
line "G11 Backup & restore"). Whole-DB export/restore per engine: Postgres
(`pg_dump`/`pg_restore`, external binaries), MSSQL (server-side
`BACKUP`/`RESTORE DATABASE`), SQLite (`VACUUM INTO` / file copy). One UI
surface over two fundamentally different mechanics.

Read before implementing: `crates/dbc-ui/src/tunnel.rs` (external-binary
orchestration pattern — spawn, stderr capture, kill-on-drop, value-errors for
a missing binary — reused here almost verbatim for `pg_dump`/`pg_restore`);
`crates/dbc-ui/src/sandbox.rs` + the G5 block in the target-UI spec (pure
SQL-generation core + "show exact SQL → confirm → transaction" dialog
pattern, reused for the MSSQL T-SQL path and the restore confirm modal);
`crates/dbc-state/src/vault.rs` + `config.rs` (secrets never on disk in
plaintext, `AppConfig` for non-secret metadata); `crates/dbc-core/src/
connection.rs` (the `execute()` write-path contract this phase extends).

## 1. Tool detection (Postgres only — MSSQL/SQLite need no external binary)

- **Persistence:** new `AppConfig.tool_paths: ToolPaths { pg_dump:
  Option<String>, pg_restore: Option<String> }` (dbc-state, `config.rs`,
  same TOML file as connections — global, not per-connection, since the
  installed tool is a machine property, not a connection property). Manual
  override always wins and is validated (file exists + is executable) at
  USE time, not at save time — a stale saved path (uninstalled since) must
  surface as a value-error, not silently fall through.
- **Detection order per tool:** (1) `AppConfig.tool_paths.<tool>` if set →
  use as-is, error if missing at spawn time (`"nakonfigurovaná cesta k
  pg_dump neexistuje: <path> — nastavte ji znovu"`); (2) PATH via `where
  pg_dump` (same probe shape as `tunnel.rs::ssh_binary`, generalized to a
  `find_on_path(name: &str) -> bool` helper); (3) glob
  `C:\Program Files\PostgreSQL\*\bin\pg_dump.exe`, picking the
  numerically-highest version directory (`*` parsed as `u32`, non-numeric
  dirs ignored, ties broken by directory mtime) — pure function
  `pick_highest_version_dir(dirs: &[String]) -> Option<String>` unit-tested
  without touching the real filesystem; (4) not found → the backup/restore
  action is still offered in the UI (so the user can discover it) but
  clicking it opens the progress modal already in the `Failed` state with
  `"pg_dump nenalezen — nastavte cestu ručně"` and a "Nastavit cestu…"
  button that opens a native file-open dialog (GPUI's, same one `grid.rs`'s
  `start_export` uses for save dialogs) and persists the chosen path to
  `AppConfig.tool_paths`.
- **`pg_dump`/`pg_restore` resolved independently** (a user may only have
  one on PATH, e.g. a client-tools-only install) — the backup flow only
  needs `pg_dump`, restore only `pg_restore` (or `psql` for a plain-SQL
  dump, see §3).
- **Version-compat check:** run once per backup/restore session, non-blocking
  UI (a status line in the progress modal, not a separate dialog).
  `pg_dump --version` parsed for its major version number; server major
  version read from `SHOW server_version_num` (Postgres always exposes it,
  cheap, no schema access needed) via the existing `query()` path on a
  throwaway connection. Decision: **block** if `client_major <
  server_major` (dumping a newer server with an older client is the
  documented-unsupported direction — PostgreSQL explicitly warns pg_dump
  should be the same version or newer than the server); **warn**
  (dismissible banner, does not block Start) if `client_major >
  server_major` by more than 2 (still supported per PG's back-compat
  policy, but old-enough servers can hit edge cases); **no message** for
  `client_major == server_major` or `client_major` up to 2 ahead. Restore
  doesn't re-check (the dump file's format is what governs compatibility;
  `pg_restore` is generally forward-compatible with older custom-format
  dumps).

## 2. Backup flow per engine

### Postgres (`pg_dump`)

- **Format:** default **custom format** (`-Fc`) — enables `pg_restore`'s
  selective restore, built-in compression, and parallel restore later if
  ever needed; **Plain SQL** (`-Fp`, writes a `.sql` file) offered as a
  radio alternative for portability/human review. Directory format (`-Fd`,
  parallel dump) is explicitly OUT of scope for G11 (adds a
  directory-vs-file target-picker split and parallel-job UI for a benefit
  — parallel dump speed — this phase doesn't need); revisit if a later
  phase needs it.
- **Compression:** `-Fc` only, `--compress=0..9` slider, default 6 (pg_dump's
  own default). Plain SQL is never compressed by the app (gzip-after would
  need its own decompression step on restore — out of scope; a user who
  wants a compressed plain dump can gzip it themselves outside the app).
- **Target file picker:** GPUI's native save dialog (same call `grid.rs`'s
  `start_export` uses), suggested filename `<connection-db>-<yyyyMMdd-
  HHmmss>.<ext>` (`.backup` for custom format, `.sql` for plain). No
  Downloads-folder fallback here (unlike export) — if the dialog fails or
  is cancelled, the action is simply aborted with a status-bar note; a
  silent fallback path for a DESTRUCTIVE-adjacent, potentially
  multi-gigabyte backup file is the wrong default.
- **Spawn:** reuses `tunnel.rs`'s shape — `Stdio::null()` stdin,
  `Stdio::piped()` stderr, `CREATE_NO_WINDOW` on Windows, dedicated struct
  (`BackupProcess`) that kills the child on `Drop`. Args: `-h host -p port
  -U user -d database --format=c|p --file=<path> --compress=N -v`
  (`-v`/`--verbose` is what makes pg_dump emit per-object progress lines to
  stderr at all — without it there's no log to show). **Password:**
  `PGPASSWORD` environment variable set on the spawned `Command`, read from
  the vault at spawn time and held only in the child's env block — never
  appended to `args` (would show in Task Manager / process list) and never
  logged (the progress log echoes the built argv for user transparency but
  the password env var is never part of argv nor logged). `.pgpass` and a
  libpq connection-URI-with-embedded-password were both considered and
  rejected: `.pgpass` requires writing a secrets file to the user's
  profile with exact permission bits the app would need to manage
  cross-platform, and a URI-with-password puts the secret in argv — the
  exact thing being avoided. `PGPASSWORD` is the standard libpq-recognized
  answer to "no interactive prompt, no disk, no argv".
- **Progress:** **honest decision — no percentage.** `pg_dump -v` prints
  object-level lines (`"pg_dump: dumping contents of table \"public.orders
  \""` etc.) with no overall total known in advance, so a progress bar
  would be fabricated. UI: a scrolling read-only log pane (each stderr
  line appended, auto-scrolled) + an indeterminate spinner + a live
  elapsed-time counter. This matches the honest posture the spec asked
  for.
- **Cancellation:** "Zrušit" button in the modal while `Running` → kills the
  child (`child.kill(); child.wait()`, same as `Tunnel::drop`) and deletes
  the (necessarily partial) output file if it exists, logging `"přerušeno
  uživatelem — částečný soubor smazán"`. Deletion is best-effort (a Windows
  file-lock release after `kill()` isn't instant; retry the unlink once
  after a short wait, then give up silently if still locked — a leftover
  partial file named with the timestamped filename is an acceptable
  failure mode, it's obviously not a valid backup by its own recency).
- **Timeout:** **no timeout.** Backups are long-running by nature and size
  is unbounded; unlike the query timeout (G1), there is no sane default
  duration. The only cancellation mechanism is the manual "Zrušit" button.
  A stalled `pg_dump` (e.g. blocked on a lock) is indistinguishable from a
  slow one from the log alone — documented as a risk (§6), not solved here.

### MSSQL (`BACKUP DATABASE`)

- Runs as ordinary T-SQL over the **existing query path**, not a new
  driver method: `BACKUP DATABASE [db] TO DISK = N'<server-path>' WITH
  FORMAT, STATS = 10` — no external binary, no `execute()` needed since it
  returns informational messages rather than a rowset or an affected-row
  count that matters to the app (see progress note below for why
  `execute()` is still the right call site regardless).
- **Path input:** a plain text field, NOT a native file picker (a native
  Windows-file-dialog would browse the CLIENT machine's disk, which is
  actively misleading here). Explicit amber warning label above the field:
  **"Cesta je na disku SERVERU MSSQL, ne na tomto počítači."** Placeholder
  text shows an example server path (`D:\Backups\mydb.bak`). No existence
  validation client-side (the app cannot see the server's filesystem) —
  errors surface from the `BACKUP DATABASE` statement itself (e.g. "Cannot
  open backup device").
- **Progress:** `STATS = 10` makes SQL Server emit `"10 percent
  processed."`-style informational messages roughly every 10%. Whether
  the app's current odbc-api usage surfaces these (TDS INFO messages,
  distinct from the result set) is **UNVERIFIED** — flagged in §6. Design
  commits to: if `dbc-driver-mssql`'s query execution already exposes a
  message/diagnostic-records callback (odbc-api's `Diagnostics`/message
  API), wire STATS=10 lines into the same scrolling log pane as the
  Postgres flow, parsing `"N percent processed"` into an actual progress
  bar (this is the ONE case in G11 that could get a real percentage, not
  guessed). If message surfacing turns out not to be reachable through the
  current abstraction, **fallback**: indeterminate spinner + elapsed timer,
  identical to the Postgres posture — never block the phase on this
  uncertainty.
- **Cancellation:** the same `CancelToken` the query path already threads
  through `Connection::query`/`execute` (`crates/dbc-core/src/cancel.rs`)
  — issuing `SQLCancel` against the ODBC statement handle. Caveat recorded
  in §6: a cancelled server-side `BACKUP`/`RESTORE` leaves file cleanup
  entirely to SQL Server; the app has no visibility into (and cannot
  clean up) a partial file on the server's disk.
- **Read-only gate:** `BACKUP DATABASE` is a **read** per the binding
  constraint (it reads the DB, writes only to disk, not to the database
  itself) — allowed even on `read_only` connections. This is the one place
  in the app where a "write-shaped" SQL statement is intentionally exempt
  from the read-only gate; implemented as a dedicated call path (§5 T5),
  NOT by relaxing the general `is_read_statement` guard main.rs uses for
  ad-hoc SQL.

### SQLite (`VACUUM INTO`)

- **Decision: `VACUUM INTO`, not a raw file copy.** A raw `std::fs::copy`
  of a live SQLite file risks copying mid-write pages (WAL mode: the
  `-wal`/`-shm` siblings would also need copying and checkpointing
  first); `VACUUM INTO 'path'` is a single SQL statement SQLite guarantees
  is a transactionally-consistent snapshot, safe against concurrent
  readers/writers on the SAME connection's session, no WAL-file juggling
  needed. Executed via `Connection::execute()` (no rows returned).
- **Overwrite:** SQLite's `VACUUM INTO` refuses to run if the destination
  file already exists (`SQLITE_ERROR: output file already exists`). The
  save dialog itself prompts for overwrite confirmation (native OS
  behavior); on confirmed overwrite the app unlinks the destination file
  first, then runs `VACUUM INTO`, so the user-facing behavior still matches
  "overwrite" even though SQLite's own primitive doesn't support it
  directly.
- **Progress:** no meaningful progress signal from `VACUUM INTO` either —
  same indeterminate-spinner-plus-elapsed-timer treatment, for consistency
  across engines even though SQLite backups are typically fast.
- **Cancellation:** via `CancelToken` into `execute()` (same mechanism as
  every other query/execute cancellation already in the app).

## 3. Restore flow per engine — DANGEROUS, all three gated identically

- **Confirm friction (applies to ALL three engines):** a modal requiring
  the user to **type the database name** to enable the "Obnovit" button
  (GitHub-delete-repo pattern) — chosen over a plain Ano/Ne confirm because
  restore is irreversible and silently destroys the current state of a
  real database; a single click is too little friction for this action
  class (same posture the spec's "DANGEROUS" callout asks for, one notch
  stronger than G5's Apply-transaction confirm since G5 edits are
  reviewable row-by-row and RESTORE is all-or-nothing). Modal shows: target
  connection name, database name (must be retyped exactly, case-sensitive),
  source backup file/path, and — for MSSQL specifically — the extra
  warning "Tímto se přeruší VŠECHNA ostatní připojení k této databázi."
- **Read-only gate:** RESTORE is a **write** — blocked entirely (menu
  item disabled, tooltip "připojení je jen pro čtení") on `read_only`
  connections, no override. Same gate `sandbox.rs`'s Apply already
  enforces for edits.

### Postgres (`pg_restore` / `psql`)

- **Binary dispatch by source format:** a **custom-format** (`-Fc`) dump is
  restored with `pg_restore`; a **plain-SQL** dump has no `pg_restore`
  path at all (it's not pg_restore's archive format) — it's restored by
  piping the file into `psql`. The app detects which by reading the
  dump's first bytes (`PGDMP` magic header for custom format; anything
  else is treated as plain SQL) rather than trusting a file extension.
- **Options (custom-format / `pg_restore`):** `--clean --if-exists` **ON by
  default** (drop existing objects before recreating them — restoring into
  an already-populated target, which is the app's normal case since
  connections point at an existing database, needs this or every CREATE
  fails); `--create` (create the database itself) **OFF by default** and
  offered as an advanced checkbox — the app's connection already names an
  existing target database in the overwhelmingly common case; `--no-owner
  --no-privileges` **ON by default** (avoids restore failures from
  role/privilege mismatches between the source and target server, a
  common friction point) with an "Advanced" disclosure to turn them off.
  `--single-transaction`/`-1` **ON by default** for custom-format restores
  — atomic (all-or-nothing) restore matching the "one transaction" posture
  G5's Apply already established; if unsupported by the target combination
  `pg_restore` itself reports the conflict and the modal surfaces it as a
  failed run (nothing partially applied).
- **Plain-SQL restore (`psql`):** no equivalent transaction flag is forced
  by the app — a plain dump from `pg_dump -Fp` already wraps its content in
  `BEGIN;`/`COMMIT;` by default (`pg_dump`'s own behavior), so the app
  just streams the file to `psql -h ... -f <file>` and shows its stdout/
  stderr in the same log pane. Note in the UI when plain-SQL restore is
  selected: "atomicita závisí na obsahu souboru" (matches reality — the
  app cannot guarantee atomicity for an arbitrary hand-edited SQL file the
  way it can for the custom-format path).
- Password: `PGPASSWORD` env var, identical mechanism to backup.
- Cancellation/timeout: identical posture to backup (kill on cancel, no
  timeout).

### MSSQL (`RESTORE DATABASE ... WITH REPLACE`)

- `RESTORE DATABASE [db] FROM DISK = N'<server-path>' WITH REPLACE, STATS
  = 10`. **`WITH REPLACE`** is required because the target database
  already exists as the connection's own database (RESTORE otherwise
  refuses to overwrite a DB with mismatched backup lineage) — this is the
  correct default for this app's per-connection model, not offered as a
  toggle (a restore that ISN'T replacing the connection's own database
  doesn't fit this UI's "restore ties to a connection" framing at all).
- **Exclusive access requirement:** SQL Server requires no other session
  be connected to the target database during RESTORE. Mechanism (dedicated
  connection, same "open a fresh connection used exclusively for this
  sequence and drop it after" pattern `connection.rs`'s doc comment
  mandates for the sandbox Apply transaction): (1) app drops its OWN
  cached `Connection` handle(s) for this connection id from `AppView`
  state first (closing any open result tabs' live connections is out of
  scope of what the app can force — see risk in §6); (2) opens one fresh
  connection and runs `ALTER DATABASE [db] SET SINGLE_USER WITH ROLLBACK
  IMMEDIATE` (forcibly disconnects any OTHER session too — hence the
  extra warning in the confirm modal); (3) `RESTORE DATABASE ...`; (4)
  `ALTER DATABASE [db] SET MULTI_USER`; (5) drops the dedicated connection.
  Step 4 always runs even if step 3 fails (best-effort `try`/finally
  shape) so a failed restore doesn't leave the database stuck in
  single-user mode.
- Progress/cancellation: as in §2's MSSQL backup entry.

### SQLite (replace file)

- **App must close its own connection(s) to the file first** — SQLite on
  Windows holds an OS-level file lock; overwriting a locked file fails
  outright (unlike Postgres/MSSQL this isn't a server-side operation the
  app merely watches, the app IS the only process usually holding the
  file open). Mechanism: (1) drop every live `Connection` the app holds
  for this connection id (same "drop the cached handle" step as MSSQL);
  (2) `std::fs::copy(backup_path, db_path)` (overwriting in place — the
  restored file's `-wal`/`-shm` siblings, if the backup itself is a raw
  copy rather than a `VACUUM INTO` output, are NOT restored — restoring
  ONLY files produced by this app's own `VACUUM INTO` backup avoids that
  edge case entirely, since `VACUUM INTO` output is always a single
  consistent file with no WAL sidecars); (3) app marks the connection
  "disconnected" in its UI state and shows a status-bar prompt to
  reconnect, rather than silently auto-reconnecting — reusing whatever
  post-disconnect UX G1's connection lifecycle already has for a dropped
  connection (no new state machine invented here). Any OPEN result/preview
  tabs for that connection become stale; they are left as-is (their data
  is a frozen snapshot already, consistent with how the app treats any
  other closed connection) rather than force-closed.
- No cancellation needed (file copy is a single fast syscall for realistic
  SQLite DB sizes; no process to kill).

## 4. UI surface

- **Entry points:** two new actions, "Zálohovat databázi…" / "Obnovit
  databázi ze zálohy…", added to (a) the connection dropdown's per-item
  context menu (right-click, same menu that already hosts folder/favourite
  actions) and (b) the connection manager's row context menu — both
  operate on a SAVED connection (SSH/vault-backed), not the ad-hoc
  CLI-URL path, since backup/restore need the full `ConnectionConfig` (host,
  port, engine) and vault secret. (c) Ctrl+K palette: two new
  `PaletteAction` variants (`BackupDatabase`, `RestoreDatabase`), shown
  only when a connection is currently active (mirrors how existing
  connection-scoped palette actions already gate on connection state).
  Restore is grayed out (still listed, disabled state + tooltip) rather
  than hidden when the active connection is `read_only`, so its existence
  is discoverable.
- **Progress/log modal:** new `ModalState::BackupRestore` variant
  (`connections_ui.rs`'s enum grows a 4th arm, rendered with the same
  `.occlude()` overlay shape as the other three) holding: `kind: Backup |
  Restore`, `engine`, a growing `Vec<String>` log (each line = one stderr/
  message line, or one synthetic status line for SQLite's single-syscall
  path), `status: Running | Succeeded | Failed(String) | Cancelled`,
  `started_at`, an internal handle to kill the process / cancel the query
  (kept out of the `Clone`-able parts of `ModalState` the same way
  `MasterPasswordPrompt`'s `Entity<TextField>` already isn't naively
  cloned — see existing modal-state patterns). Same Esc-closability rule
  as other modals: NOT closable while `Running` (closing while a backup is
  mid-flight would orphan the child process from the UI's perspective —
  the "unsaved state blocks Esc" precedent already exists for the
  password-prompt modal). "Zrušit" while running; "Zavřít" once terminal.
- **History entries:** reuses `dbc-state::history.rs`'s existing `entries`
  table rather than adding a new one — smallest change, and the History
  panel already renders arbitrary SQL text so a synthetic descriptive line
  displays sensibly with zero new UI. Migration: `ALTER TABLE entries ADD
  COLUMN kind TEXT NOT NULL DEFAULT 'query'` (idempotent `IF NOT EXISTS`-
  style guard the same way `history.rs::open` already guards its `CREATE
  INDEX IF NOT EXISTS`), values `'query' | 'backup' | 'restore'`. The
  `sql` column stores a synthetic, secret-free description, e.g. `-- BACKUP
  demo -> D:\backups\demo-20260823-141200.backup (pg_dump -Fc, compress=6)`
  or `-- RESTORE demo <- D:\backups\demo.backup (pg_restore --clean
  --if-exists)`; `duration_ms` and `error` populate exactly as a query run
  would; `row_count` is always `NULL` (not applicable). History panel
  renders `kind != 'query'` rows with a small badge/icon (🗄) ahead of the
  text so they're visually distinguishable at a glance — the only new
  render-side change needed.

## 5. Task decomposition

- **T1 — Tool detection (dbc-state + dbc-ui), pure + persistence.**
  `ToolPaths` struct + `AppConfig` field/migration; pure
  `pick_highest_version_dir`; PATH-probe helper generalized from
  `tunnel.rs::ssh_binary`. Tests: pure unit tests over synthetic dir
  lists (no real filesystem/registry needed), config roundtrip test
  mirroring `config.rs`'s existing `roundtrip_save_load`. No dependency on
  other tasks.
- **T2 — Shared orchestration skeleton (dbc-ui, new `backup.rs`).**
  `BackupProcess`/log-streaming plumbing generalized from `tunnel.rs`'s
  spawn/kill-on-drop shape (stderr → channel → `cx.spawn`-consumed log
  lines, same shape the query-cancel path already uses for streaming);
  `ModalState::BackupRestore` variant + render skeleton (no engine logic
  yet — a stub that can display a canned log). Blocks T3/T4/T5/T6/T7's
  final wiring but its SIGNATURE can be stubbed early so T3–T6 develop
  against it in parallel.
- **T3 — Postgres backup (`pg_dump` orchestration).** Arg-building is a
  pure function (`build_pg_dump_args(cfg, opts) -> Vec<String>`, unit
  tested for exact argv incl. quoting/format/compress flags, PGPASSWORD
  NEVER appears in the returned `Vec<String>`) + the spawn/stream/cancel
  wiring using T2. Depends on T1 (tool path) + T2 (skeleton); independent
  of T4/T5/T6.
- **T4 — Postgres restore (`pg_restore`/`psql` dispatch).** Pure
  `detect_dump_format(bytes) -> Custom | Plain` (magic-header sniff, unit
  tested on canned byte slices) + pure `build_pg_restore_args`/
  `build_psql_args` + spawn wiring reusing T2/T3's process shape. Depends
  on T1, T2; independent of T3/T5/T6 (parallel-safe once T2's skeleton
  lands).
- **T5 — MSSQL backup/restore (T-SQL builders + `execute()`
  orchestration).** Pure `build_backup_sql`/`build_restore_sql`/
  `build_single_user_sql` (using `dbc_core::quote_ident` for db name,
  same quoting helper `sandbox.rs` already uses — string-literal quoting
  for the file path via the existing `sql_value`-style `''`-doubling),
  fully unit tested without a live server. Orchestration: the
  dedicated-connection SINGLE_USER/RESTORE/MULTI_USER sequence (best-
  effort MULTI_USER-on-failure), plus updating `connection.rs`'s
  `execute()` doc comment to name backup/restore as its second sanctioned
  caller (currently says "ONLY the sandbox Apply flow may call it" —
  needs a documented amendment, not a silent violation). Depends on T2;
  independent of T3/T4/T6. STATS=10 message-surfacing spike is inside
  this task (§6 risk) — falls back to spinner-only without blocking the
  rest of the task if the odbc-api message path isn't reachable.
- **T6 — SQLite backup/restore (`VACUUM INTO` + file replace).** Own-
  connection-close mechanism (drop cached `Connection` handles for a
  connection id — needs a small addition to whatever connection-lifecycle
  registry `connect.rs` already keeps, if one doesn't already expose a
  "drop by id" operation) + `VACUUM INTO` via `execute()` + file-copy
  restore. Depends on T2; independent of T3/T4/T5.
- **T7 — UI wiring.** Context-menu entries, palette actions, the typed-
  name confirm modal (pure `confirm_matches(typed: &str, expected: &str)
  -> bool` — trivial but unit tested for exact-match/case-sensitivity
  same rigor as everything else here), wiring the engine-specific
  backends (T3–T6) behind one dispatch point keyed on `Engine`. Depends on
  T2's skeleton for early stubbing but its FINAL form depends on T3–T6.
- **T8 — History integration.** `kind` column migration +
  `HistoryDb::add`-equivalent call sites from the backup/restore
  completion handlers; History panel badge rendering. Independent of
  T3–T6's internals (only needs "backup/restore finished, here's a
  description string + duration + error") — can be developed in parallel
  against T2's stub completion callback.

**Parallelization:** T1 and T2 first (T2 can start immediately, doesn't
need T1). Once T2's skeleton signature is fixed, T3, T4, T5, T6, and T8 all
proceed in parallel (five independent workstreams — matches this repo's
existing pattern of per-engine driver work, e.g. `dbc-driver-postgres` /
`dbc-driver-sqlite` already developed independently). T7 is the integration
tail, landing after T3–T6 are individually merged.

**Testing shape overall:** every module here follows the `tunnel.rs`/
`sandbox.rs` split already established in this codebase — pure
argument-building, format-sniffing, and confirm-matching functions get full
unit-test coverage with zero process/filesystem/network dependency; the
THIN spawn/stream/kill layer around them gets a small number of integration
tests using a fake/trivial subprocess (same trick as `tunnel.rs`'s
`missing_binary_is_a_value_error`, extended with one real-spawn test using a
universally-available command — e.g. Windows' `cmd /C echo` looped a few
times with sleeps to simulate multi-line stderr streaming — rather than
requiring a real `pg_dump`/SQL Server install in unit tests; the true
external-tool paths are exercised by manual/integration testing against a
real Postgres/MSSQL instance, same as the existing driver crates' `tests/
integration.rs` pattern already does for `dbc-driver-postgres`).

## 6. Risks / needs-verification

- **NEEDS VERIFICATION:** whether the current `dbc-driver-mssql` (or
  odbc-api usage generally, once that driver lands — MSSQL driver phase is
  orthogonal per the target-UI spec's phasing table) surfaces TDS
  informational messages (the channel `STATS = 10` writes to) through
  whatever query-execution abstraction exists at G11 implementation time.
  If not reachable, MSSQL backup/restore progress silently degrades to
  spinner-only (§2/§3) — not a blocker, but the "real percentage" upside
  may not materialize.
- **Partial-file cleanup on cancel is best-effort on Windows** — a killed
  `pg_dump` may hold its output file locked briefly after `kill()`
  returns; the app retries the unlink once and gives up silently after
  that, potentially leaving an obviously-partial file named with a
  now-stale timestamp.
- **MSSQL `SINGLE_USER WITH ROLLBACK IMMEDIATE` forcibly disconnects every
  other session** on that database, including other users on a shared
  server — this is inherent to `RESTORE`'s requirements, not an app choice,
  but is a real operational risk; mitigated only by the confirm modal's
  explicit warning, not by any technical safeguard (the app has no way to
  detect "other sessions exist" before pulling the trigger without itself
  querying `sys.dm_exec_sessions`, which is a nice-to-have surfaced-in-the-
  confirm-modal enhancement, not designed in detail here).
- **No cross-app coordination for SQLite's "close own connections"
  step** — if the same SQLite file is also open in a DIFFERENT process
  (e.g. the user has it open in another tool, or a second instance of this
  app), the app can only close ITS OWN handles; the OS-level file lock from
  the other process still blocks the restore copy, surfacing as a plain
  I/O error from `fs::copy`. Documented, not solved.
- **No timeout on any backup/restore operation** (§2) — a stalled operation
  (lock contention, network stall to a remote MSSQL/Postgres server) is
  only recoverable via manual cancellation; there's no automatic
  stuck-detection. Consistent with treating these as fundamentally
  long-running, size-unbounded operations, but worth re-examining if this
  becomes a real support burden after ship.
- **`VACUUM INTO` under concurrent write load** is expected to be safe per
  SQLite's documented guarantees, but has not been exercised in this
  codebase against a WAL-mode database under simultaneous writes from
  another connection — flagged for a manual check during implementation
  rather than assumed correct from documentation alone.
- **`execute()`'s doc-comment contract** ("ONLY the sandbox Apply flow may
  call it") is now stale as of this design — G11 is a second caller
  (MSSQL backup/restore's T-SQL, SQLite's `VACUUM INTO`/nothing-for-restore-
  since-that's-a-file-copy). T5 must update that comment; flagged here so
  the discrepancy isn't mistaken for an oversight if spotted mid-review
  before T5 lands.
