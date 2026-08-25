# Workspace Folder (git-versioned working context) + Scripts Library — Design

Date: 2026-08-25 (widened; supersedes `scripts-library-design.md`, whose
scripts part is RETAINED verbatim as Part S below, original § numbering)

Status: designed per the WIDENED user decision (binding, do not
relitigate). The phase was originally "scripts library" (a folder of
`.sql` files, Bruno model). The user widened it 2026-08-25: „to nejsou
jen sql skripty ale vsechno, connection, user nastaveni atd." — ONE
user-chosen, git-versioned **workspace folder** holds the entire working
context. Confirmed contents (user selected ALL): connections
(„pripojeni vcetne hesel"), app settings, SQL scripts, view prefs /
query params. Password handling: the user explicitly chose **„Šifrovaný
trezor do složky"** — the encrypted Argon2id vault file lives IN the
versioned folder — over the never-version-secrets recommendation, after
being warned once. Unchanged from the original scope: **git stays
EXTERNAL** — no git engine, no git credentials, no commit/push/status
UI in the app, ever.

Orchestrator-mandated safety requirements this design implements:
the vault is a SEPARATE file (trivially `.gitignore`-able without losing
anything else, §W6.1); an honest in-app warning where the folder is
configured (git history is permanent, security = master-password
strength, keep the repo private — §W6.3); a decided position on the
`.gitignore` template (ship it, §W6.2) and on public-remote detection
(static warning, NO `.git` inspection, §W6.4); existing security rules
still bind (passwords ONLY in the vault, config files never contain
secrets, no secret in logs/history — §W6.5).

Read before implementing (in addition to Part S's list):
`crates/dbc-state/src/config.rs` (`default_config_path`,
`AppConfig::load`/`save`, `no_password_field_serialized`),
`crates/dbc-state/src/vault.rs` (`Envelope` — salt+KDF params travel in
the file; `default_vault_path`), `crates/dbc-state/src/view_prefs.rs` /
`params.rs` / `history.rs` (all stores take `&Path`),
`crates/dbc-ui/src/main.rs` `fn main()` ~9116–9155 (the ONLY place
default paths are resolved; `AppView.config_path`/`vault_path` fields
1090/1097), `crates/dbc-mcp/src/main.rs` `parse_args` (existing
`--config`/`--vault` overrides), and the app-wide-master-password
design's §0 survey (vault session model: lazy prompt, at most once per
run).

---

## Part W — Workspace folder

### W0. Grounding facts the design leans on

1. **Every persistent store is already path-parameterized.** `AppConfig
   ::load/save(&Path)`, `Vault::create/unlock/exists(&Path)`,
   `ViewPrefsStore::load(&Path)`, `ParamValuesStore::load(&Path)`,
   `HistoryDb::open(&Path)`. The `default_*_path()` free functions are
   called in exactly TWO places: `dbc-ui/main.rs::main()` and
   `dbc-mcp/main.rs::parse_args()`. Workspace mode is therefore a
   **path-resolution change at two call sites**, not a storage rework.
2. **The profile dir today** (`dirs::config_dir()/dbc/`, on Windows
   `%APPDATA%\dbc\`): `config.toml`, `vault.bin`, `history.sqlite`,
   `views.toml`, `params.toml`. Nothing else.
3. **The vault file is self-contained.** The JSON envelope carries
   `kdf`, `m_cost/t_cost/p_cost`, `salt`, `nonce`, `ciphertext` — a
   copy of `vault.bin` on another machine unlocks with the same master
   password, no side channel needed. Tampering/wrong password fail
   closed (AEAD). This is what makes the user's chosen model work at
   all.
4. **The vault session model is app-wide and lazy** (master-password
   design §0): `AppView.vault: Option<Vault>` starts `None`, the
   prompt fires at most once per run, on first secret use — not at
   startup. Workspace mode changes only WHICH file the prompt unlocks.
5. **`AppView` owns `config_path` + `vault_path`**; history/view-prefs/
   params stores carry their own path internally from construction.
   A mode switch = construct new stores + replace fields (§W3.4).
6. **dbc-mcp** resolves the same defaults and already accepts
   `--config`/`--vault` path overrides; its keyring entry stores an
   exported vault KEY (machine-local by nature, Windows Credential
   Manager).
7. **No secrets in config files is a tested invariant**
   (`no_password_field_serialized`) — copying `config.toml` into a
   git-versioned folder leaks no secret by construction.

### W1. Workspace folder layout (decided)

```
<workspace>/                  ← the ONE user-chosen folder (git repo root, typically)
  dbc-workspace.toml          ← marker + format version (format = 1); no settings
  config.toml                 ← AppConfig: connections, favourites, theme, tool_paths
  vault.bin                   ← encrypted Argon2id vault — SEPARATE file, §W6.1
  views.toml                  ← per-table view prefs (ViewPrefsStore)
  params.toml                 ← last-used :param values (ParamValuesStore)
  scripts/                    ← the .sql tree (Part S), created on init
  .gitignore                  ← generated ONCE at init (template, §W6.2)
```

Decisions and rationale:

- **Identical file names to the profile dir.** Every store already
  takes a `&Path`; identical names make initialization a plain file
  copy (§W3.2), make the folder self-describing to a human reading the
  repo, and mean zero format work — the on-disk formats, their
  back-compat tests, and the atomic tmp+rename writers are reused
  byte-for-byte.
- **A dedicated marker file `dbc-workspace.toml`** (content: exactly
  `format = 1`) rather than treating `config.toml` presence as the
  signal. Rationale: (a) unambiguous folder classification — any
  directory can contain a `config.toml`; (b) a place for a format
  version so a future layout change can fail politely („pracovní
  prostor vyžaduje novější verzi aplikace") instead of misreading;
  (c) the adopt flow (§W3.3) can distinguish "workspace" from "random
  non-empty folder" without heuristics. It carries NO settings —
  settings live in `config.toml` so profile/workspace parity holds and
  nothing is read from an unauthenticated second channel.
- **`scripts/` is a fixed subfolder convention** in workspace mode.
  `AppConfig.scripts_dir` (T1) remains the PROFILE-mode setting; in
  workspace mode it is inert (§W8) — the scripts tree always roots at
  `<workspace>/scripts/`. A per-workspace override would reintroduce
  absolute paths into a folder whose whole point is portability.
- **`history.sqlite` deliberately stays in the profile dir** — see
  §W5.
- The app never writes anything else into the folder root. Foreign
  files (`README.md`, `.git/`, CI configs…) are invisible and
  untouched — same posture as Part S §1.5.

### W2. Mode resolution — pointer file, no merging (decided)

**The pointer lives OUTSIDE the workspace**, in the profile dir:
`%APPDATA%\dbc\workspace.toml`, content `path = "D:\\..."` (absolute).
A dedicated file, NOT a field in the profile `config.toml`. Rationale:
if the pointer lived inside `AppConfig`, workspace mode would read the
profile config for one field while ignoring the rest — a "which fields
are still live" ambiguity that invites divergence bugs. With a
dedicated pointer, the rule is absolute:

> **Exactly one context is active. No merging, ever.**
> Pointer file present and valid ⇒ ALL five path slots resolve into the
> workspace (except history, §W5); the profile `config.toml`/
> `vault.bin`/`views.toml`/`params.toml` are completely inert.
> Pointer file absent ⇒ today's behavior, unchanged to the byte.

Startup resolution (`dbc-state::workspace` module, §W9 task W1):

```rust
pub enum Paths { /* config, vault, views, params, history: PathBuf */ }
pub enum Resolution {
    Profile(Paths),                       // no pointer — today's behavior
    Workspace { root: PathBuf, paths: Paths },
    /// Pointer exists but the target is missing/unreadable/not a
    /// workspace (no marker, or format > supported). The app must NOT
    /// silently fall back — §W4.
    Broken { pointer: PathBuf, root: PathBuf, reason: String },
}
pub fn resolve() -> Resolution;
```

Precedence questions answered by construction: there ARE no precedence
or merge rules — a datum lives in exactly one active file. If both a
profile config and a workspace config exist on disk (they will, after
init — §W3.2 copies, never moves), the inactive one is simply never
read. The settings UI states which mode is active („Pracovní prostor:
{path}" vs „Lokální profil") so the user is never guessing.

### W3. Choosing / initializing / adopting a workspace

Settings modal („Nastavení") gains a „Pracovní prostor" block above
„Složka skriptů" (which it subsumes in workspace mode, §W8):

- Profile mode: „Lokální profil ({profile dir})" + button „Použít
  složku…" (`prompt_for_paths { directories: true }`).
- Workspace mode: „Pracovní prostor: {path}" + button „Přejít na
  lokální profil" (clears the pointer after the same gate checks as a
  switch; workspace files stay on disk untouched).
- The static security warning (§W6.3) renders in this block in BOTH
  modes' folder-selection flow.

On folder pick, classify (in `workspace.rs`, background-dispatched):

1. **`dbc-workspace.toml` present** ⇒ **adopt** (§W3.3) — this is the
   machine-B / second-checkout case.
2. **Effectively empty** (no entries, or only dot-entries like `.git`,
   `.gitignore` — a fresh clone of an empty repo qualifies) ⇒
   **initialize** (§W3.2).
3. **Non-empty without marker** ⇒ refuse: „error: složka není pracovní
   prostor dbc a není prázdná — vyberte prázdnou složku nebo existující
   pracovní prostor". Rationale: never scatter app files into
   `~/Documents` by misclick; never adopt a folder we cannot vouch for.
4. Marker present but `format` > 1 ⇒ refuse: „error: pracovní prostor
   vyžaduje novější verzi aplikace".

#### W3.1 Common gates (both init and adopt)

The switch is a context replacement, so it demands a quiet app — same
gate style as `start_script_pick`: no modal open beyond Settings, no
run in flight, no pending apply/discard, and the Part S §5.5 dirty
script guard runs first. The active connection (if any) is
disconnected as part of the switch — the connection list itself is
about to change; keeping a session from the OLD context alive under
the NEW config would be exactly the silent-context-mixing this design
bans. The confirm modal says so: „Aktivní připojení bude odpojeno."

#### W3.2 Initialize (empty folder): COPY, never move — never destructive

Confirm modal „Vytvořit pracovní prostor" shows the target path, the
full security warning (§W6.3), and „Aktivní připojení bude odpojeno."
Buttons „Rozumím, vytvořit" / „Zrušit"; **Enter is inert** (ScriptRun-
confirm posture — this is a deliberate, security-relevant decision, the
button is the gate). On confirm, in `cx.background_spawn`:

1. Copy the profile files that exist — `config.toml`, `vault.bin`,
   `views.toml`, `params.toml` — into the folder (each via tmp+rename;
   a missing source is skipped, e.g. a user who never saved a
   password has no `vault.bin`).
2. Create `scripts/`.
3. Write `.gitignore` (template, §W6.2) — ONLY if none exists.
4. Write `dbc-workspace.toml` (`format = 1`) LAST — the marker's
   presence is the commit point; a crash mid-init leaves a folder the
   classifier calls non-empty-without-marker, which the user can
   simply delete (nothing was moved, the profile is intact).
5. Write the pointer file in the profile dir (atomic tmp+rename).
6. On the UI thread: swap the live context (§W3.4).

**The profile files are never deleted, moved, or modified by init.**
They become inert (§W2) but remain a complete, working fallback —
„Přejít na lokální profil" restores them as-is. This is the
never-destructive requirement, satisfied structurally rather than by a
backup step.

#### W3.3 Adopt (marker present): switch, prompt for THAT vault's password lazily

Confirm modal „Otevřít pracovní prostor" shows the path, „Aktivní
připojení bude odpojeno.", and a short line: „Trezor tohoto prostoru se
odemyká jeho vlastním master heslem." No files are written except the
pointer (step 5 above), then swap (§W3.4). The workspace's
`config.toml` load follows the existing corrupt-config posture (error
surfaced, saves refused) — adopt does NOT validate content beyond the
marker; the existing loaders are the validators.

#### W3.4 The live swap (no restart mechanism exists — swap in place)

One function, UI thread, after gates: set `config_path`/`vault_path` to
the new resolution; `self.vault = None` (a workspace vault is a
DIFFERENT file — the session unlock, if any, must not carry over; the
existing lazy-prompt flow re-prompts on first secret use, at most once,
per the master-password design); replace `config` with the
freshly-loaded one (load ran in the background step); reconstruct
`view_prefs`/`params` stores from the new paths (their existing
load-or-None degrade posture); `history` untouched (§W5); clear
scripts tree state to `NotLoaded` and re-root it (§W8); status:
„pracovní prostor: {path}" / „lokální profil obnoven". The reverse
switch („Přejít na lokální profil") is the same function with the
profile resolution, gated and confirmed the same way (it deletes the
pointer file, touching nothing in the workspace).

### W4. Missing/moved workspace at startup — fail loudly (decided)

If `resolve()` returns `Broken` (pointer set; folder missing, marker
gone, unreadable, or future format), the app starts with an EMPTY
default config and a **blocking startup modal** — not Esc-closable, no
other UI interactive behind it (`ModalState::WorkspaceMissing`,
policy-table clause: Enter inert, Esc inert):

> „Pracovní prostor nenalezen
> {path}
> {reason — e.g. „složka neexistuje" / „chybí dbc-workspace.toml"}"

Three buttons, all explicit:

- „Najít složku…" — re-pick; ONLY a folder with a valid marker is
  accepted here (this flow is "the workspace moved", not "make a new
  one"); on success, rewrite the pointer and proceed via §W3.4.
- „Použít lokální profil" — an explicit, logged-in-status user action:
  deletes the pointer, continues with profile files. The modal text
  above this button says plainly: „Otevře se lokální profil — jiná
  připojení a nastavení než v pracovním prostoru."
- „Ukončit" — quit the app.

**Never a silent fallback**: without the pointer's target, the app must
not quietly show profile-mode connections — the user would be one
muscle-memory click from running a query against the wrong context's
idea of „prod". The empty-config start ensures even a bug that
dismissed the modal would leave nothing to connect to.

Corrupt-but-present workspace files are NOT this modal: they follow
the existing per-store degrade postures (config: status-bar error +
save refusal; views/params: feature off; those are content errors
inside the right context, not a wrong-context risk).

### W5. What stays machine-local, and honoring the user's "everything"

The user selected connections, settings, scripts, view prefs / query
params for the folder — all four go in, including `tool_paths` inside
`config.toml` (machine-specific in nature, but it is part of settings
the user chose to version; on machine B a stale `pg_dump` path fails
with the existing clear error and is fixed in Settings — disclosed in
§W7, not silently split out). `views.toml`/`params.toml` go in as
chosen; their keys are connection ids, which the workspace also owns,
so they are genuinely portable — this is the case where following the
user's instruction is also the technically better design.

**Two things stay in the profile dir, with stated rationale:**

- **`history.sqlite`** — the user did not select history for the
  folder, and it is the one store that would actively fight git:
  a binary SQLite file rewritten on every query is a guaranteed
  conflict factory and repo-bloat engine. History remains per-machine.
  (Reversible later as its own decision; recorded, not smuggled.)
- **The pointer file `workspace.toml`** — by construction it cannot
  live in the folder it points to.

Window geometry is not persisted anywhere today (fixed
`Bounds::centered(1200×800)`), so the "machine-local window size"
question is moot — nothing exists to split.

### W6. Security posture (orchestrator requirements + existing rules)

#### W6.1 The vault is a separate file — gitignorable without loss

`vault.bin` is its own file (it already was; this design keeps it that
way and REQUIRES it to stay that way — never fold secrets into
`config.toml` or the marker). Consequence, documented in the warning
and the `.gitignore` template: a user can add one line (`vault.bin`)
to `.gitignore` and keep versioning everything else; the vault then
lives only in working copies. Nothing else in the folder references
vault content; no other file breaks when it is ignored or absent
(missing vault = the existing "no vault yet" flows: connect prompts
for a password / create-vault on first save).

#### W6.2 Generated `.gitignore` template — SHIP IT (decided)

Written once by init (§W3.2, never overwritten, never touched again —
it is the user's file from the moment it exists):

```gitignore
# dbc workspace — pracovní prostor aplikace dbc.
# Git zde spravujete výhradně vy; aplikace s gitem nikdy nepracuje.

# Dočasné soubory atomických zápisů (po pádu aplikace mohou zůstat):
*.toml.tmp
*.bin.tmp

# DOPORUČENÍ: vault.bin je šifrovaný trezor hesel (Argon2id).
# Pokud ho NECHCETE verzovat (bezpečnější volba), odkomentujte
# následující řádek. POZOR: historie gitu je trvalá — jednou
# commitnutý trezor z ní nelze spolehlivě odstranit.
# vault.bin
```

Rationale: the commented-out `vault.bin` line makes the opt-out a
one-character-delete discovery at exactly the place a git user looks;
the active `*.tmp` lines are pure hygiene (crash leftovers of the
tmp+rename writers); comments change no git behavior, so shipping the
template carries zero risk of overriding user intent. Not shipping it
would make the "trivially possible" requirement depend on the user
knowing the filename.

#### W6.3 The in-app warning (exact copy, decided)

Shown (a) inside the init/adopt confirm modals and (b) statically in
the Settings „Pracovní prostor" block while the folder-pick flow is
offered:

> „Upozornění: složku verzujete sami — git zůstává zcela mimo
> aplikaci. Historie gitu je trvalá: jednou commitnutý trezor
> (vault.bin) z ní nelze nikdy spolehlivě odstranit. Bezpečnost celé
> složky se pak rovná síle vašeho master hesla. Repozitář držte
> privátní, nebo vault.bin vyřaďte z verzování (.gitignore ve složce
> má připravený zakomentovaný řádek)."

Honest, specific, once-per-decision-point; no nagging on every
startup (the user made this call knowingly — the warning exists so the
call stays informed, not to relitigate it).

#### W6.4 Public-remote detection — NOT done; static warning instead (decided)

Considered: reading `<workspace>/.git/config` (plain file, read-only)
to warn when a remote looks public. Rejected, three reasons:
(a) **the user's line**: "git stays external" is recorded in code as
"the app never reads or writes anything git-related about this folder"
(T1's `scripts_dir` doc comment) — parsing `.git/config` breaches the
spirit even if it is not an "engine", and the orchestrator's guidance
prefers a static warning when in doubt; (b) **the heuristic is
misleading both ways**: a `github.com` remote URL says nothing about
repo visibility (most are private) — a warning that cries wolf on
private repos teaches the user to ignore it, and determining actual
visibility would need network/API access, far over the line;
(c) the static §W6.3 warning already carries the operative advice
(„repozitář držte privátní") unconditionally, which is strictly more
reliable than a guess. The app therefore never opens anything under
`.git/` — which the Part S scan rules already guarantee for the
scripts tree (dot-dirs never descended).

#### W6.5 Existing invariants, restated as gates for this phase

- Passwords exist ONLY inside `vault.bin` (Argon2id + ChaCha20-
  Poly1305). No new file in the workspace may carry a secret; the
  marker, pointer, and `.gitignore` are static/near-static text; the
  `no_password_field_serialized` test keeps guarding `config.toml`,
  and the workspace copy is the same serializer.
- No secret in logs, status lines, or history — unchanged; the switch
  flows log paths and mode names only.
- The vault file's own crypto posture is unchanged (fail-closed AEAD,
  KDF-param caps against corrupt envelopes, key/plaintext zeroization)
  — versioning copies of the file does not weaken any of it beyond
  the disclosed history-is-permanent property.

### W7. Multi-machine story (documented behavior)

Machine B, fresh: install dbc → `git clone` the repo → Settings →
„Použít složku…" → pick the checkout → classified as **adopt** (marker
present) → immediately working: connections list, favourites, theme,
scripts tree, view prefs, param prefills. First action needing a
password (connect to a non-SQLite engine) triggers the existing lazy
master-password prompt — the SAME master password as machine A, because
salt and KDF params travel inside `vault.bin` (fact W0.3). Nothing else
is needed.

Known per-machine seams, disclosed rather than papered over:

- `tool_paths` may point at machine-A paths (§W5) — backup/restore
  features error clearly until fixed in Settings; the fix is itself
  versioned on the next commit, which cuts both ways (documented; the
  `Option` + auto-detect fallback means `None` users never notice).
- Concurrent edits on two machines merge at the GIT layer, by the
  user, per file: `config.toml`/`views.toml`/`params.toml` are TOML and
  merge textually; **`vault.bin` cannot be merged** (encrypted
  envelope) — a conflict is resolved by picking one side wholesale
  (ours/theirs), and the losing side's newly-added passwords must be
  re-entered. Stated in the design and the release notes; the app
  cannot and does not try to help (that would be the rejected git
  engine).
- `history.sqlite` does not travel (§W5).
- **dbc-mcp**: the keyring-stored exported key is machine-local by
  design — machine B runs `dbc-mcp setup` once. dbc-mcp learns to
  resolve the SAME pointer file in `parse_args` (default paths become
  workspace paths when the pointer is valid; explicit `--config`/
  `--vault` flags still win). Broken pointer ⇒ dbc-mcp exits with the
  error, same fail-loudly rule as §W4 — it must not silently serve
  profile-mode connections either.

### W8. Absorbing the scripts phase (T1/T2 landed; T3–T6 redirected)

- **T1 (`AppConfig.scripts_dir`, branch `sc-t1-config`) — absorbed
  as-is.** It remains the PROFILE-mode scripts root: a scripts-only
  user who never adopts a workspace keeps the Part S §2 behavior
  unchanged. In workspace mode the field is **inert**: the scripts
  root is always `<workspace>/scripts/` (§W1); the Settings „Složka
  skriptů" block is replaced by a fixed read-only line „Skripty:
  {workspace}\scripts" (no picker, no „Odebrat"), and the app never
  writes `scripts_dir` while in workspace mode. Inertness is
  documented on the field's doc comment (a hand-edited `scripts_dir`
  in a workspace `config.toml` is ignored — one root per mode, no
  precedence question).
- **T2 (`crates/dbc-ui/src/scripts.rs`, branch `sc-t2-fsmod`) —
  absorbed unchanged.** Every function takes `root: &Path`; the
  workspace merely supplies a different root. All Part S safety rails
  (rel-path validation, symlink skip, caps, atomic writes) apply
  identically.
- **T3–T6 (not yet built)** proceed per Part S with ONE seam: the
  scan/ops root comes from a single resolver
  (`fn effective_scripts_root(&self) -> Option<PathBuf>` — workspace
  mode ⇒ `Some(root.join("scripts"))`, profile mode ⇒
  `config.scripts_dir`). The unconfigured-notice row (Part S §1.4)
  can only appear in profile mode; in workspace mode the folder
  always exists (init created it; if the user deletes it, the scan
  error row + retry covers it honestly).
- The Part S §9 task table's T1/T2 rows are DONE; the remaining
  ordering is restated in §W9.

### W9. Task decomposition (ordering for the plan pass — plan not yet written)

Serialization constraint unchanged: `main.rs`/`connections_ui.rs`/
`schema_tree.rs` are single-writer; dbc-state and dbc-mcp are separate
crates and can proceed in parallel lanes.

| # | Content | Crate/files | Depends on |
|---|---|---|---|
| T1✓ | `scripts_dir` field (landed, `sc-t1-config`) | dbc-state | — |
| T2✓ | `scripts.rs` fs module (landed, `sc-t2-fsmod`) | dbc-ui/scripts.rs | — |
| W1 | `dbc-state::workspace`: pointer read/write, marker, folder classification, `resolve()`, init-copy (steps §W3.2 1–5 as pure fns over paths), full tempfile suite incl. crash-ordering (marker-last) and never-touch-profile assertions | dbc-state/workspace.rs | — (∥ T3) |
| T3 | Scripts sidebar model (Part S §3, dark) | schema_tree.rs | T2 |
| T4 | Scripts flip: settings row, scan wiring — root via `effective_scripts_root` stub (profile arm only, workspace arm lands in W2) | main.rs, connections_ui.rs | T1, T3 |
| T5 | Editor binding (Part S §5) | main.rs | T4 |
| T6 | Script mutations + run reuse (Part S §4/§6) | main.rs, connections_ui.rs | T5 |
| W2 | Startup resolution: `resolve()` at `main()`, `WorkspaceMissing` blocking modal (§W4), workspace arm of `effective_scripts_root` | main.rs, connections_ui.rs | W1, T6 |
| W3 | Settings „Pracovní prostor" block, classify + init/adopt confirm modals with §W6.3 warning, `.gitignore` template, live swap §W3.4, reverse switch | main.rs, connections_ui.rs | W2 |
| W4 | dbc-mcp pointer awareness (§W7) + NONE-diff regression canary stays green | dbc-mcp | W1 (∥ W2–W3) |
| W5 | Sweep: docs as-built, memory, release notes (vault-merge + tool_paths disclosures), version bump, full gates + smoke | Cargo.toml, docs | all |

Scripts tasks first (T3–T6), workspace tasks after (W2–W3): the scripts
UI is self-contained and already planned; the workspace flip then
re-roots it in one seam. Versioning: this is now ONE widened phase —
one minor bump at W5 (0.22.0; re-verify free at merge).

### W10. What this phase deliberately does NOT do (recorded)

- No git engine, subprocess, credentials, status/commit/diff UI, and —
  per §W6.4 — not even read-only `.git/` inspection. Permanent.
- No multi-workspace switcher/recents; ONE pointer, one active
  context. (A recents list is a cheap follow-up if asked.)
- No merging of profile + workspace state, no partial precedence — the
  design's central invariant.
- No auto-migration of `history.sqlite` into the folder (§W5).
- No file-level encryption of anything beyond the vault; no attempt to
  "help" with vault git conflicts (§W7).
- No watcher on the workspace folder (Part S §1.2 posture applies to
  all of it: external `git pull` ⇒ ⟳ refresh for scripts; config/prefs
  files are read at startup/switch — a mid-session external edit of
  `config.toml` is out of scope, same as today's profile mode).
- No workspace-level settings in `dbc-workspace.toml` (marker stays a
  pure marker).

---

# Part S — Scripts Library (retained design, original numbering)

Status: RETAINED from the 2026-08-25 scripts-library design pass;
binding decisions unchanged (pure Bruno model — plain `.sql` files, app
shows a tree; git EXTERNAL, no git UI ever; scripts never
auto-executed). Where the workspace widening touches a section, a
**[WIDENED]** note marks the delta; everything else stands as written.
T1 and T2 below are IMPLEMENTED (branches `sc-t1-config`,
`sc-t2-fsmod`).

Read before implementing: `crates/dbc-ui/src/schema_tree.rs`
(`SidebarRow`, `OuterId` + its polarity note, `flatten_sidebar`,
`emit_schema_slot`, `toggle_outer`, `handle_chevron`/`handle_single_click`
/`handle_double_click`, the inline icon rows ★/⊞/⇪, the Notice color
dispatch on the `"error:"` prefix); `crates/dbc-ui/src/main.rs`
(`start_script_pick` 2600–2750 — the G12 confirm flow this design REUSES,
`list_sql_files` 399, `count_statements_in_file` 374, the editor column
8728–8760, `actions!` 67, `bind_keys` 9157, `DiscardConfirmState`/
`PendingDiscard` 988–1046); `crates/dbc-ui/src/sql_input.rs`
(`SqlInput::text`/`set_text`); `crates/dbc-state/src/config.rs`
(`AppConfig`, the `ToolPaths` additive-field precedent + paired
back-compat tests, `AppConfig::save`'s tmp+rename atomic write);
`crates/dbc-ui/src/connections_ui.rs` (`ModalState`,
`modal_confirm_kind`'s exhaustive policy table, `render_settings_panel`
1626, `TextField`).
## 0. Grounding facts the design leans on

1. **Tabs are RESULT tabs only.** `TabContent` has nine variants — Grid/
   Text/Monitor/Plan/Diagram/Compare/Chart/ScriptRun/Admin — and none is
   an editor. The SQL editor is ONE global `Entity<SqlInput>`
   (`AppView.sql`, main.rs:1071) rendered as a fixed 8-line pane ABOVE
   the tab strip. There is no per-tab editor state, no editor dirty
   tracking, no file binding, and no Ctrl+S/Ctrl+O binding anywhere
   (verified: repo-wide grep). This forces §3's central adaptation.
2. **The G12 script runner already runs `.sql` files from disk** with a
   mandatory confirm modal (statement-count pre-scan, tx-scope/error-
   policy radios, Enter deliberately inert) and a per-statement
   read-only gate in the runner. It has NO notion of a scripts folder
   and NO recent list — files come from an ad-hoc `prompt_for_paths`
   pick each time. This design gives those files a home; the run path
   itself is reused verbatim (§6).
3. **The sidebar is a multi-root `uniform_list`** built by the pure
   `flatten_sidebar`; pinned root sections („Správa serveru",
   „Oblíbené (n)") already coexist with connection roots, so a new
   pinned root section is the established shape — not a new panel.
   There are NO context menus and NO tooltips at the pinned GPUI rev;
   row actions are always-rendered inline icon divs (★/⊞/⇪ precedent),
   and there is no inline-rename anywhere (modals are the precedent).
4. **Config lives in `%APPDATA%\dbc\config.toml`** (`AppConfig`), new
   fields are additive `#[serde(default)]` with paired back-compat
   tests; `ToolPaths` is the existing PATH-type-setting precedent
   (paths in config.toml are fine — they are not secrets). A settings
   modal exists (`ModalState::Settings`, „Nastavení", theme-only today)
   and is the natural home for the folder setting.
5. **No `notify`, no `walkdir` in the dependency tree** (walkdir only
   transitively via GPUI). Directory walking is hand-rolled
   `std::fs::read_dir` (`list_sql_files` precedent), always dispatched
   off the UI thread via `cx.background_spawn`.
6. **GPUI file dialogs have no extension filter** at the pinned rev
   (resolved G12 spike, main.rs:2626) — `.sql` enforcement is always
   client-side.

## 1. Decisions the phase brief left open — resolved

### 1.1 One global folder (chosen) vs per-workspace/per-connection

**ONE global folder**, stored as `AppConfig.scripts_dir: Option<String>`
(§2). Rationale: (a) Bruno's own model is one collection folder opened
by path; (b) the app has no workspace concept, and per-connection
folders would multiply empty trees and settings UI for a v1 nobody
asked for; (c) the user who wants per-server organisation just makes
subfolders — the tree renders them natively. **Documented convention,
not enforced:** a subfolder per connection name (e.g.
`prod-pg/reporting.sql`) — the app never creates, names, or interprets
such folders.

**[WIDENED]** In workspace mode the root is ALWAYS
`<workspace>/scripts/` and `scripts_dir` is inert — see §W8. Everything
in this section about the tree itself is unchanged; only where the root
comes from differs (`effective_scripts_root`).

### 1.2 File-watching vs manual refresh

**No `notify` watcher this phase.** The refresh story is:
- automatic rescan after EVERY in-app mutation (create/rename/delete/
  save-as — §5), so the app's own actions are never stale;
- a `⟳` icon on the „Skripty" root row (manual, e.g. after a
  `git pull`);
- automatic scan on startup (when configured), on folder (re)selection,
  and on expanding the root while `NotLoaded`/`Error`.

Rationale: `notify` is a new dependency with per-platform watcher
threads, debounce tuning, and event-storm handling — real complexity
purchased to save one click after an external edit. The scan itself is
one bounded background `read_dir` walk (≤ 2000 entries, §7), i.e.
milliseconds; a stale tree is self-healing and harmless because every
file operation re-validates against the real filesystem at dispatch
time (a vanished file yields a Czech error, never corruption).
"Refresh-on-window-focus" was considered and rejected too: the pinned
GPUI rev's activation-observer surface is unproven in this codebase and
the ⟳ affordance covers the git-pull story honestly. Watcher support is
recorded as a possible follow-up, not a debt.

### 1.3 Editor relation — bind the GLOBAL editor, not per-script tabs

The brief sketched "opening a script = editor tab bound to the file",
but grounding fact 0.1 says the app has no editor tabs at all — tabs
are results, the editor is one global pane. Building a per-tab editor
model would be an editor-architecture rework (that is the g6-editor-pro
draft's territory), not a scripts-library feature. **Resolved: opening
a script binds the single global editor to the file:**

- `AppView.script_binding: Option<ScriptBinding { path: PathBuf,
  saved_text: String }>` — `path` is ABSOLUTE (survives a scripts-dir
  change; display re-relativizes against the current root and falls
  back to the file name).
- A thin caption strip renders above the editor ONLY when bound:
  „Skript: {rel}" plus the „ •" dirty suffix (the exact tab-title
  convention), an „Uložit" button (dim when clean) and „Zavřít"
  (unbind). Dirty = `sql.text() != saved_text` (exact compare, bounded
  by the 1 MiB open cap §7; length short-circuits first).
- **Ctrl+S** (new `SaveScript` action — the chord is free, verified):
  bound → atomic save (§5); unbound → save-as into the library (§5.4).
- Ctrl+Enter semantics are UNCHANGED: it runs the editor TEXT through
  the normal query path (auto-limit, params, multi-statement unlock) —
  binding never changes what runs. Running the FILE goes through the
  tree's ▶ and the G12 confirm modal (§6), and always runs the DISK
  content — a dirty binding means editor and disk differ, which the •
  makes visible; the confirm modal's from-disk statement count is the
  honest number. (Documented, not "fixed": auto-saving before run
  would be a silent write.)
- Dirty-discard guards (§5.5) protect the binding against silent text
  replacement: opening another script, closing the binding, changing
  the scripts folder, and the two existing history/palette
  "load SQL into editor" sites all route through one guarded helper.
  Note the baseline: today the editor text is clobbered with NO guard
  anywhere; this phase strictly improves that for bound scripts and
  leaves unbound ad-hoc text exactly as guarded as before (not at
  all).
- App exit with a dirty binding is NOT guarded this phase (the app has
  no exit interception anywhere; adding one is out of scope). Same
  posture as today's editor text, disclosed in release notes.

### 1.4 Where the tree lives

**A third pinned root section „Skripty" in the existing sidebar**,
emitted after „Oblíbené" and before the CLI/connection roots, collapsed
by default. Rationale: fact 0.3 — pinned sections are the established
multi-root shape; a separate panel would cost fixed-width real estate
(the 260 px sidebar is not resizable), a new toggle, and a second
uniform_list for zero interaction gain. The section is GLOBAL: unlike
★/⊞/⇪ it does not depend on the active scope and renders its icons
unconditionally (scripts are files, not database objects; running one
is where connection context enters, via the existing G12 gates).
The section renders even when `scripts_dir` is unset — expanding it
shows one clickable notice row „složka skriptů není nastavena —
klikněte pro Nastavení" that opens the settings modal (discoverability
without a wizard).

### 1.5 `.sql` filter — show only `.sql` (chosen)

The tree shows folders and `*.sql` files (case-insensitive), nothing
else — matching `list_sql_files` and Bruno's only-`.bru` posture. The
library is a query library, not a file manager; a `README.md` or
`.git/` in the folder is invisible and untouched. (`.git` specifically:
it is just another non-matching directory — the scan descends into it
never, see §7's dot-dir rule.) Disclosed in the design, not in-UI.

## 2. Config + settings UI

**[WIDENED]** This section is the PROFILE-mode behavior (implemented as
T1, branch `sc-t1-config`). In workspace mode the „Složka skriptů" block
is replaced by a fixed read-only line and the picker/„Odebrat" do not
render — §W8.

```rust
// config.rs — AppConfig gains (ToolPaths precedent):
/// Scripts library (Bruno model): absolute path of the user-chosen
/// folder with plain `.sql` files. `None` = feature dormant (the
/// sidebar section shows a pointer to Settings). A path, not a secret —
/// config.toml is the right home. Git integration is deliberately
/// EXTERNAL (user decision 2026-08-25): the app never reads or writes
/// anything git-related about this folder.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub scripts_dir: Option<String>,
```

Paired back-compat tests per house convention (old file loads with
`None` + roundtrip stays byte-identical until set).

Settings modal gains a „Složka skriptů" block under „Motiv": the
current path (or „nenastavena") in muted text, buttons „Vybrat složku…"
(`prompt_for_paths { directories: true }`; on pick: store the absolute
path, save config, dispatch a scan) and „Odebrat" (set `None`, save,
clear the tree state; a dirty binding routes through the §5.5 guard
first — the binding itself survives, since it holds an absolute path,
but the guard fires when the SECTION removal would strand a dirty
buffer with no tree affordance — resolved: „Odebrat" only clears tree
state and never touches the binding; no guard needed, the caption
strip's „Uložit" still works). „Zavřít"/Esc semantics unchanged.

## 3. Tree model

### 3.1 New module `crates/dbc-ui/src/scripts.rs` (pure + std-fs only)

```rust
/// One entry of the scanned library, in DISPLAY order (depth-first;
/// within a directory: folders first, then files, each name-ordered).
/// `rel` uses '/' separators on all platforms (stable expand keys and
/// event payloads; resolved back to components by `resolve_rel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptEntry {
    pub rel: String,
    pub is_dir: bool,
    pub depth: usize, // 0 = direct child of the root
}

pub struct ScriptScan {
    pub entries: Vec<ScriptEntry>,
    pub truncated: bool,     // entry cap hit (SCRIPTS_ENTRY_CAP)
    pub depth_clipped: bool,  // depth cap hit (SCRIPTS_DEPTH_CAP)
}

pub const SCRIPTS_ENTRY_CAP: usize = 2000;  // 2000-db cap precedent
pub const SCRIPTS_DEPTH_CAP: usize = 12;
pub const SCRIPT_OPEN_CAP: u64 = 1_048_576; // 1 MiB editor-open cap
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

All errors are Czech display strings (runner precedent). `write_script`
is atomic (`.tmp` + `sync_all` + `rename` — the `AppConfig::save`
shape). `create_*`/`rename_entry` return the NEW rel path.

### 3.2 Scan rules (§7 carries the safety rationale)

- Iterative walk (explicit stack of `(PathBuf, rel_prefix, depth)`), no
  recursion — house rule.
- Entries: directories (except names starting with `.` — this is what
  keeps `.git/` etc. invisible AND undescended) and files with a
  case-insensitive `.sql` extension. Everything else skipped silently.
- **Symlinks are skipped entirely** (checked via
  `fs::symlink_metadata().file_type().is_symlink()` before descending
  or listing): a symlinked directory could walk outside the chosen
  root or cycle; a symlinked file's target is equally out-of-root.
  One rule, zero traversal exposure.
- Per-directory ordering: folders first, then files, each by
  case-insensitive name; depth-first splice so the output is already
  display order.
- Caps: stop emitting past `SCRIPTS_ENTRY_CAP` total entries
  (`truncated`); do not descend past `SCRIPTS_DEPTH_CAP` (`depth_clipped`);
  each gets its own Notice row (§3.4).

### 3.3 Sidebar state + rows

`SchemaTree` gains one slot (same state-machine family as `DbListState`):

```rust
pub enum ScriptsListState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { entries: Vec<ScriptEntry>, truncated: bool, depth_clipped: bool },
}
```

`SidebarRow` gains variants (all matches are exhaustive — compiler-
guided sweep):

```rust
ScriptsRoot,                       // pinned section header „Skripty"
ScriptFolder { rel: String },
ScriptFile { rel: String },
ScriptNotice { text: String, open_settings: bool }, // notices; click opens Nastavení when flagged
```

`OuterId` gains `Scripts` and `ScriptFolder(String)` (both default
COLLAPSED — presence in the set = expanded, the lazy polarity;
folders-default-open applies to CONNECTION grouping folders only, the
scripts tree is lazy like connections).

`flatten_sidebar` takes one new parameter
`scripts: Option<(&ScriptsListState, bool)>` (`None` until the flip
task passes state; the bool = „scripts_dir configured"). Emission: the
„Skripty" root row after the Oblíbené block; when expanded —
unconfigured ⇒ the settings-pointer notice; `Loading` ⇒ „Načítám
skripty…"; `Error(e)` ⇒ `error: {e}` retry row; `Loaded` ⇒ entries
whose ancestor folders are all expanded, at `1 + entry.depth`, plus cap
notices. Speed search: same contract as everything else — filters
LOADED rows only, never fetches; folders auto-expand under an active
filter and childless non-matching rows truncate away (existing
pattern).

### 3.4 Czech strings (binding)

- Section: „Skripty"; empty loaded root: „žádné skripty (*.sql)".
- Unconfigured notice: „složka skriptů není nastavena — klikněte pro
  Nastavení".
- Loading: „Načítám skripty…"; errors: `error: {msg}` (color sentinel).
- Cap notices: „… zobrazeno prvních 2000 položek — zmenšete knihovnu
  skriptů" and „… některé podsložky jsou příliš hluboko (limit 12
  úrovní)".
- Settings: „Složka skriptů", „nenastavena", „Vybrat složku…",
  „Odebrat".
- Caption strip: „Skript: {rel}" (+„ •"), „Uložit", „Zavřít".
- Statuses: „skript uložen: {name}", „skript vytvořen: {name}",
  „přejmenováno: {name}", „smazáno: {name}", „error: nastavte složku
  skriptů v Nastavení".

## 4. Interactions (rows, icons, events)

Inline icon divs (★/⊞/⇪ precedent: always rendered, `stop_propagation`,
then emit):

| Row | Icons | Click | Double-click |
|---|---|---|---|
| ScriptsRoot | `⟳` refresh, `+` new item at root | select | toggle expand |
| ScriptFolder | `+` new item here, `✎` rename, `✕` delete (empty only) | select | toggle expand |
| ScriptFile | `▶` run (G12 flow), `✎` rename, `✕` delete | select | open into editor (§5.1) |
| ScriptNotice | — | retry scan / open Nastavení | — |

New `TreeEvent` variants (handlers land with the flip — the exhaustive
match forces same-task): `ScriptsRefresh`, `OpenScriptsSettings`,
`ScriptOpen { rel }`, `ScriptRunFile { rel }`,
`ScriptCreate { parent_rel }`, `ScriptRename { rel, is_dir }`,
`ScriptDelete { rel, is_dir }`.

Modals (both new `ModalState` variants; `modal_confirm_kind` is
exhaustive and each must pick a policy side):

- `ScriptName { mode, parent_rel, target_rel, is_dir, field:
  Entity<TextField>, error: Option<String> }` — one dialog for new
  script / new folder / rename („Nový skript" with a Skript/Složka
  radio; „Přejmenovat"). Enter = confirm (creates/renames a FILE, runs
  nothing against the database — policy table clause (a)). Esc closes.
- `ScriptDeleteConfirm { rel, is_dir }` — „Smazat {skript|složku}
  {name}? Akce je nevratná (maže se z disku, ne do koše)." Buttons
  „Smazat"/„Zrušit". Enter = **Ignore** (§3-novela spirit: the button
  is the last gate before an irreversible action — the action targets
  the filesystem, not the database, but the rule's substance is
  irreversibility, not SQL). Esc closes.

All fs mutations dispatch through `scripts.rs` ops in
`cx.background_spawn`, then rescan on success; errors land in the
modal (`error` field) or status line. Rename/delete of the currently
BOUND file fix the binding up: rename updates `binding.path`; delete
clears the binding (its dirty guard runs FIRST — deleting a dirty-bound
file prompts the §5.5 discard confirm before the delete confirm even
opens — resolved simpler: the delete confirm text gains a second line
„Skript má neuložené změny v editoru." when it targets the dirty-bound
file; one modal, both facts).

## 5. Editor binding mechanics

### 5.1 Open (`TreeEvent::ScriptOpen`)

Guarded by §5.5. Resolve `resolve_rel(root, rel)`; background: stat
size (> `SCRIPT_OPEN_CAP` ⇒ „error: soubor je příliš velký pro editor
(limit 1 MiB) — spusťte jej jako skript"), read to `String` (lossy
UTF-8 conversion is an error, not a mangle: non-UTF-8 ⇒ „error: soubor
není platné UTF-8"); then on the UI thread: `sql.set_text`, set
`script_binding = Some(ScriptBinding { path, saved_text })`, status
cleared. Opening never runs anything (brief: script files are user
content — never auto-execute).

### 5.2 Save (Ctrl+S / „Uložit", bound)

Capture `(path, text)`; background `write_script` (atomic tmp+rename);
success ⇒ `saved_text = text`, status „skript uložen: {name}"; failure
⇒ `error: {e}`. Last-writer-wins on external edits — by the user's own
model, git is the history/merge layer; the app does not diff or
version (that is exactly the rejected variant).

### 5.3 Close binding („Zavřít")

Guarded by §5.5; sets `script_binding = None`, editor text stays (it
is just no longer bound).

### 5.4 Save-as (Ctrl+S unbound)

Empty editor ⇒ no-op status „editor je prázdný". No `scripts_dir` ⇒
„error: nastavte složku skriptů v Nastavení". Else
`prompt_for_new_path(&root, Some("dotaz.sql"))`; append `.sql` when
missing (client-side, fact 0.6); write, bind, and rescan when the
saved path is inside the root (outside is allowed — it is the user's
disk — but the tree honestly won't show it).

### 5.5 One dirty guard

`fn editor_load_guarded(&mut self, action: PendingScriptAction, cx)` —
when `script_binding` is dirty, park the action in the existing
`DiscardConfirmState` machinery via a new
`PendingDiscard::Script(PendingScriptAction)` arm (message branches:
„Neuložené změny skriptu {name} budou zahozeny."); else perform
immediately. Actions: `Open { rel }`, `Unbind`, `LoadText { sql }`
(the history-panel and palette history-click sites route here) —
deleting a dirty-bound file needs NO action here, §4 resolved it to a
second line inside the delete confirm. The guard NEVER protects unbound
ad-hoc text —
identical exposure to today, zero behavioral regression surface.

## 6. Running a saved script — G12 reuse, verbatim gates

`TreeEvent::ScriptRunFile { rel }` runs the EXISTING confirm flow with
the picker stage replaced by the known path:

1. Same entry gates as `start_script_pick` (no modal/apply/discard
   open, no run in flight, spec resolves, dialect exists), same
   `conn_identity` capture BEFORE the pre-scan.
2. Background: `resolve_rel`, extension re-check, existence check
   (stale tree ⇒ Czech error + rescan), `count_statements_in_file`.
3. The post-pre-scan continuation of `start_script_pick` (modal-races
   + identity re-check + `ModalState::ScriptRun` construction) is
   FACTORED into one helper both paths call — the scripts library must
   not fork the confirm policy. Everything downstream (`confirm_script_run`'s
   re-checks, `script_run_dispatch_allowed`, tx/error radios, the
   runner's per-statement read-only gate, progress tab, history's
   `[skript]` synthetic entry) is untouched by construction.

The ad-hoc „SQL soubor…"/„SQL složku…" buttons and palette actions stay
— they serve out-of-library files; no affordance is removed.

## 7. Safety rails (the audit)

1. **Root escape:** every fs op goes through `resolve_rel`, which
   splits on `/`, rejects empty/`.`/`..` components and any component
   containing `\`, `:`, control characters, or a Windows drive/UNC
   shape, then joins onto the root. Rels only ever ORIGINATE from the
   scan (which builds them from single `file_name()` components) or
   from `validate_script_name` output — the check is defense in depth,
   pinned by tests (`..`, `a/../b`, `C:\x`, `\\srv\share`, `x\0y`).
2. **Symlinks:** skipped at scan (never descended, never listed —
   §3.2), therefore never openable/renamable/deletable via the tree.
   No canonicalize-and-compare dance needed: nothing out-of-root ever
   gets a rel.
3. **Hostile filenames (created in-app):** `validate_script_name` —
   trim; non-empty; ≤ `SCRIPT_NAME_CAP`; no `/ \ : * ? " < > |`, no
   control chars; no leading/trailing dot or space; case-insensitive
   reserved-device check (CON, PRN, AUX, NUL, COM1–9, LPT1–9, also
   with any extension); files get `.sql` appended when missing (then
   re-validated). Collision ⇒ „název už existuje" (pre-checked
   case-insensitively via the scan snapshot + `try_exists`).
4. **Hostile filenames (from disk):** rendered as text in GPUI rows
   (no interpolation into SQL/paths beyond the join in 1); lossy
   display is acceptable, operations use the real `OsString` path
   carried by `resolve_rel`'s join.
5. **Large folders:** `SCRIPTS_ENTRY_CAP = 2000` + disclosure Notice
   (the 2000-db precedent verbatim); `SCRIPTS_DEPTH_CAP = 12` +
   disclosure; the walk is iterative, so depth is a cap policy, not a
   stack-safety need.
6. **Large files:** `SCRIPT_OPEN_CAP = 1 MiB` for EDITOR opens only —
   running via ▶ streams through the G12 splitter in 64 KiB chunks and
   has no such cap (unchanged).
7. **Never auto-execute:** open = read text; save/rename/delete never
   touch a database connection; run = explicit ▶ + the unchanged G12
   confirm modal + runtime read-only gates. No path from scan to
   execution exists.
8. **No new secrets:** the only new persisted datum is a folder path in
   config.toml (ToolPaths posture). No git credentials, no git
   subprocess, no network. dbc-mcp is untouched (its config read gains
   an inert optional field; the merge gate proves NONE diff).
9. **Deletes are explicit and bounded:** files only after the confirm
   modal; folders only when EMPTY („složka není prázdná — smažte
   nejdřív její obsah") — no recursive delete in v1 (git can restore a
   file; an un-tracked recursive delete cannot be undone by us).

## 8. What this phase deliberately does NOT do (recorded)

- No git engine/status/commit/diff UI — permanently out (user
  decision, restated so no future phase "helpfully" adds badges).
- No `notify` watcher (§1.2 — possible follow-up, not debt).
- No per-tab editors / multi-open scripts (g6-editor-pro territory).
- No recursive folder delete; no drag-drop move; no duplicate/copy.
- No palette items per script (would require the palette to hold the
  scan; follow-up candidate) — the palette gains only „Uložit skript".
- No app-exit dirty guard (no exit interception exists app-wide).
- No MCP exposure of the library.
- No `.sql` templates/snippets, no non-`.sql` files in the tree.

## 9. Task decomposition (serialization explicit)

**[WIDENED]** Superseded by §W9, which absorbs this table (T1/T2 are
DONE on `sc-t1-config`/`sc-t2-fsmod`; T3–T6 stand as specified here
with the `effective_scripts_root` seam; T7's sweep merges into W5).
Kept for reference:

| T | Content | Files (owner) | Depends on |
|---|---|---|---|
| T1 ✓done | dbc-state: `scripts_dir` + back-compat tests | dbc-state/config.rs | — |
| T2 ✓done | `scripts.rs`: scan/validate/resolve/fs ops + full tempfile suite | dbc-ui/scripts.rs (+1 `mod` line in main.rs) | — |
| T3 | schema_tree additive: rows, `OuterId`, `ScriptsListState`, flatten emission (dark: `scripts: None`), expand/click plumbing, pure tests | schema_tree.rs (+its flatten call sites) | T2 |
| T4 | FLIP 1 — settings row, scan dispatch wiring, section live, ⟳/notice events | connections_ui.rs, main.rs | T1, T3 |
| T5 | Editor binding: open/save/close/save-as, Ctrl+S, caption strip, §5.5 guard, palette entry | main.rs, sql_input read-only usage | T4 |
| T6 | Mutations + run: name/delete modals, fs dispatch, binding fixups, ▶ → factored G12 confirm | main.rs, connections_ui.rs | T5 |
| T7 | Sweep: docs as-built, memory, version 0.22.0, full gates + smoke | Cargo.toml, docs | T6 |

`main.rs` and `connections_ui.rs` serialize T4 → T5 → T6; T1 ∥ T2 up
front. Versioning: one minor bump — **0.22.0** (verify free on main at
merge time, house convention).

## 10. Self-review notes

- Checked against fact 0.1: no decision assumes per-tab editors; the
  binding is the minimal honest adaptation and the brief's "editor tab"
  intent (dirty tracking, Ctrl+S to file, discard guards) is fully
  preserved on the global editor.
- Checked the G12 seam: reuse is by FACTORING the existing
  continuation, not by a parallel modal — one confirm policy site.
- Checked exhaustiveness blast radius: `SidebarRow`, `OuterId`,
  `TreeEvent`, `ModalState`, `modal_confirm_kind`, the Esc-closable
  match, and the tab-strip content match (unchanged — no new
  TabContent) — each lands with its arms in the same task.
- Rejected alternatives, for the record: separate scripts panel
  (§1.4); `notify` watcher (§1.2); per-connection folders (§1.1);
  recursive delete (§7.9); auto-save-before-run (§1.3); rfd dependency
  for filtered dialogs (client-side `.sql` checks are the established
  workaround); storing rel paths in the binding (breaks on root
  change).
