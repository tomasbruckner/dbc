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

   **AS-BUILT ADDENDUM (workspace T5) — this copy was EXTENDED, and the
   short form above is no longer the binding text.** An init that crashed
   or failed part-way leaves exactly this shape (contents copied, marker
   not written — §W3.2 step 4 makes that the crash-safe outcome by
   design), and the classify-driven flow then refuses it for BOTH init (it
   is not empty) and adopt (it has no marker), with no in-app way out. The
   short form reads „you picked the wrong folder", so the user never tries
   the one thing that works. The shipped refusal appends: „…; pokud v této
   složce dříve selhalo nebo bylo přerušeno vytváření prostoru, zůstaly v
   ní nedokončené soubory aplikace — smažte obsah složky a zkuste to
   znovu". The app still deletes nothing itself: the never-destructive
   rail holds even for files the app created.

   **AS-BUILT ADDENDUM (workspace T5) — where that refusal is DELIVERED.**
   At ~230 characters it does not fit the status bar (one unwrapped flex
   row, behind the modal backdrop), which would have truncated away the
   half that matters. The prose renders inside the Settings „Pracovní
   prostor" block; the status bar carries only a short sentinel
   („error: vybranou složku nelze použít — podrobnosti v Nastavení").
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

**AS-BUILT ADDENDUM (workspace T5) — „no modal open beyond Settings" is
as-built „beyond Settings AND the confirm modal itself".** The sentence
above, taken literally, makes the confirm button refuse itself. The gate is
deliberately RE-RUN at confirm time (the folder pick and its classification
did not block the app, so a query or a dialog may have started meanwhile),
and it necessarily runs with `ModalState::WorkspaceConfirm` on screen. The
two modals that ARE the switch flow — `Settings`, where it is started, and
`WorkspaceConfirm`, where it is confirmed — therefore do not count as „some
other dialog"; every other variant does. The exclusion lives in one
exhaustively-matched predicate (`connections_ui::modal_blocks_context_switch`)
so a new `ModalState` is a compile error that must pick a side.

**AS-BUILT ADDENDUM (workspace T5) — the gate's ORDER is pinned, and it
contradicts this section's „runs first".** §W3.1 says the dirty-script
guard runs first; the shipped `context_switch_refusal` reports a running
query first, then pending edits, then a stray dialog, and a test pins that
order. Task 8, which adds the dirty-script arm, MUST reconcile the two
deliberately — either this sentence or that ordering has to give.

**AS-BUILT ADDENDUM (workspace T8) — RESOLVED: the pinned ordering wins, and
the sentence above gives.** As built, `context_switch_refusal(run_in_flight,
dirty_script, pending_edits, other_modal_open)` reports, in order: the
running query, the dirty script, the pending edits, the stray dialog. The
section's „the Part S §5.5 dirty script guard runs first" is hereby
**amended to „…is part of this gate, ahead of every other unsaved-work
condition"**. Rationale, recorded so it is not re-litigated:

1. The gate is **all-or-nothing** — every condition blocks the switch
   equally, and `context_switch_blocked` returns the first non-`None`. The
   order therefore decides only WHICH single Czech sentence the user reads
   when several conditions hold at once. „Runs first" was never a safety
   property of this gate, and reading it as one would have been the actual
   error.
2. Under that reading the pre-existing rule is the better one and stands: a
   running query is the most immediate holder of a LIVE resource — the very
   connection this switch is about to disconnect (see the paragraph above)
   — and the one that resolves itself if the user simply waits. Sending a
   user to save a `.sql` buffer while their query is still streaming points
   them at the less urgent of two problems. (Precision, T8 review NIT-1: it
   is not the *only* live-resource condition. `pending_edits` is
   `apply_dialog.is_some() || discard_confirm.is_some()`, and an
   `apply_dialog` with `running: true` is a live write transaction on that
   same connection. It is not separable from the staged-edit state it
   travels with, and both are reported by one sentence, so the ordering is
   unaffected — but the claim is „most immediate", not „only".)
3. §W3.1's actual intent is honoured in full, and given the strongest
   reading the live-resource rule leaves: `dirty_script` is checked FIRST
   among the unsaved-work conditions, ahead of `pending_edits`. Losing a
   hand-written script is worse than losing staged grid rows, which the
   sandbox can regenerate.

The refusal text is „skript má neuložené změny — nejprve jej uložte nebo
zavřete". `AppView::context_switch_blocked` feeds it from
`script_binding.is_some() && script_dirty_flag` — the per-frame flag, not a
live `cx` read, because the gate deliberately takes no `cx`; see that
field's doc comment for why one frame of staleness can only err toward
refusing.

**AS-BUILT ADDENDUM (workspace T5) — `run_in_flight` is not every DB
operation.** The gate reads `AppView::cancel`, which `start_lookup`
deliberately does not set, so an FK-join lookup can be in flight while the
gate reports a quiet app. Benign today only by ordering: `apply_context`
clears `active_connection_id` before any completion can land, and
`save_view_prefs_for_grid` early-returns on `None`, so no write from the old
context reaches the new one. Recorded so nobody leans on it unchecked.

#### W3.2 Initialize (empty folder): COPY, never move — never destructive

Confirm modal „Vytvořit pracovní prostor" shows the target path, the
full security warning (§W6.3), and „Aktivní připojení bude odpojeno."
Buttons „Rozumím, vytvořit" / „Zrušit"; **Enter is inert** (ScriptRun-
confirm posture — this is a deliberate, security-relevant decision, the
button is the gate). On confirm, in `cx.background_spawn`:

**AS-BUILT ADDENDUM (workspace T5) — „Zrušit" is INERT while the write
runs, and so is Esc.** Once the confirm has dispatched, the background
job's success arm calls `apply_context` unconditionally; letting the modal
close mid-write would therefore swap the entire working context AFTER the
user asked to cancel — the silent context change this design bans.
Cancelling the WRITE itself is not offered, because it cannot be honoured:
the copy is already under way and nothing is ever deleted. The same
`running` latch is the double-click guard.

**AS-BUILT ADDENDUM (workspace T5) — the folder pick's continuation is
generation- AND modal-guarded.** The platform picker is modal to the app,
but the classification that follows it is not: it yields the UI thread, and
Settings is Esc-closable. A stale classification landing afterwards must
never raw-assign the modal — it could unlatch a `running` confirm, bypass
`close_modal` over a live `pg_restore` session, or wipe a half-typed
connection dialog. Two refusals, deliberately distinct: a SUPERSEDED pick
(the context already swapped) is inert and silent, the §W4 posture; a pick
that is merely in the wrong modal refuses with „výběr složky zahozen — je
otevřený jiný dialog", matching `start_script_pick` and `start_csv_import`.

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

**AS-BUILT ADDENDUM (workspace T4 review, MINOR-6) — the swap must also
drop the CONNECTION schema cache, not just the scripts tree.** The list
above named only the scripts tree, which was an omission in this spec,
not in the implementation. `SchemaTree::sync_connections` deliberately
RETAINS every cached `DbListState`/schema snapshot whose connection id
still exists — and profile and workspace contexts share ids **by
construction**, because §W3.2 initialises a workspace by copying
`config.toml` verbatim. Without an explicit reset the sidebar therefore
renders context A's databases, schemas and tables underneath context B's
identically-id'd connection after a swap. It is display-only (every query
rebuilds its connection from the new config, so nothing is ever RUN
against the wrong server) but a wrong-context display is still a wrong
context, and it is exactly the muscle-memory hazard §W4 argues about.
`apply_context` therefore calls `SchemaTree::reset_fetched_context`
BEFORE `sync_connections` re-seeds the map. Reset: per-connection database
lists and their snapshots, the snapshot LRU, the active-context schema
fallback, the CLI slot, and the selected row. Deliberately NOT reset:
expand state (holds no fetched data — it simply re-renders as
`NotLoaded`) and the sidebar filter (live user input).

**AS-BUILT ADDENDUM (workspace T4 review, MAJOR-1) — the swap is
generation-guarded.** Every `apply_context` bumps a pick generation, and
the §W4 „Najít složku…" recovery flow captures it at dispatch. Its folder
classification runs off the UI thread and can take seconds on a network
share, while the window stays interactive — so the user can reach a
different, EXPLICIT decision („Použít lokální profil") in the meantime. A
continuation whose generation is stale, or whose `WorkspaceMissing` modal
is no longer open, commits nothing. The pointer write was moved out of the
background step and onto the UI thread behind that same guard, so a
superseded pick leaves nothing persisted to undo.

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

**AS-BUILT ADDENDUM (workspace T4 re-verify) — „Enter is inert" means „no
DEFAULT button", not „Enter does nothing anywhere in this modal".** The
three choices are real tab stops, and a choice that has been tabbed to
activates on Enter as well as Space — the platform convention for pressing
a focused button. This needed a fix, not just a doc: `dispatch_key_event`
runs keymap bindings BEFORE `on_key_down` listeners and stops at the first
consumer, so the ancestor `ModalForm`'s `enter → ModalConfirm` (whose
`Ignore` arm is a handled no-op) was swallowing Enter before the button's
own listener ran, and only Space worked. The buttons now carry a deeper
`WorkspaceChoice` key context with its own `enter` binding, which
out-ranks the ancestor by `KeyBindingContextPredicate::depth_of`. The modal
still OPENS with focus on the panel container, where that context is not on
the dispatch path at all — so a bare Enter before any Tab still reaches
`ModalConfirmKind::Ignore` and does nothing, and there is still no default
button.

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

# Dočasné soubory atomických zápisů (po pádu aplikace mohou zůstat).
# Aplikace je vždy pojmenuje <soubor>.tmp, proto jediné pravidlo:
*.tmp

# DOPORUČENÍ: vault.bin je šifrovaný trezor hesel (Argon2id).
# Pokud ho NECHCETE verzovat (bezpečnější volba), odkomentujte
# následující řádek. POZOR: historie gitu je trvalá — jednou
# commitnutý trezor z ní nelze spolehlivě odstranit.
# vault.bin
```

**AS-BUILT ADDENDUM (workspace T8 re-verify) — this block was STALE and is
now corrected.** It enumerated `*.toml.tmp` / `*.bin.tmp`; the shipped
`dbc_state::workspace::GITIGNORE_TEMPLATE` has been a blanket `*.tmp` since
commit `6036961`, because `fsutil::write_atomic` names its scratch file
`<path>.tmp` for whatever it is handed — so `*.sql.tmp` (a crash during
Ctrl+S in the scripts library, by far the most frequent) and even
`.gitignore.tmp` are possible, and an enumeration silently stops covering
the next store that is added. The constant is byte-pinned by its own test;
this block is now a copy of it, not a rival source. Recorded because the
stale text was read as authoritative during Task 8 and produced a
factually wrong justification in code, since corrected.

Rationale: the commented-out `vault.bin` line makes the opt-out a
one-character-delete discovery at exactly the place a git user looks;
the active `*.tmp` line is pure hygiene (crash leftovers of the
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

**AS-BUILT ADDENDUM (workspace T5) — how „carries no git command" is
tested.** The warning necessarily contains the word „git" („git zůstává
zcela mimo aplikaci"), so the guard test bans actual git SUBCOMMANDS
(`git add|commit|push|init|clone|rm|filter`), plus URLs and credential
shapes — and asserts the word „git" REMAINS, since the warning is about
git. An earlier substring ban on `"git "` was self-contradictory against
the byte-pinned copy.

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

---

# Jak to nakonec je (as-built) — v0.22.0

Written by Task 10 (the sweep) once T1–T9 had landed and been reviewed.
Everything above is the INTENT; this section is the TRUTH. Where the two
disagree, this section wins, and the next phase should read it first.

Landed on `feature/scripts-library` as **v0.22.0** (`[workspace.package]`
in the root `Cargo.toml`; the window title reads `dbc v0.22.0`).

## A. Deviations from the design text

1. **Task ordering inverted.** The workspace lane went in before the
   scripts flip, so `effective_scripts_root` landed COMPLETE (both arms)
   in one seam instead of §W9's profile-only stub.
2. **`Resolution::Broken` carries `{ root: Option<PathBuf>, reason: String }`**,
   not §W2's sketch `{ pointer, root, reason }`. The pointer path is a
   process-wide constant (`workspace::pointer_path()`), and `root` must be
   optional because an unparsable pointer names no folder.
3. **The blocked start uses `blocked_paths`**, not profile paths. §W4 said
   "starts with an EMPTY default config" without saying which paths
   `AppView` then holds. Answer: paths inside the unusable workspace, or —
   when the pointer named nothing usable, or named the profile dir itself —
   inside a never-created sentinel folder
   (`%APPDATA%\dbc\__pracovni-prostor-nenalezen__`), so a stray save fails
   loudly instead of overwriting a profile the user did not choose.
4. **`apply_context` also clears `conn_url`** (the CLI-arg root). §W3.1 said
   only "the active connection is disconnected"; the CLI session belongs to
   the old context, so it goes with it — and per the sidebar design it
   cannot come back.
5. **Result tabs SURVIVE a context swap.** §W3.4 enumerates what the swap
   replaces and does not list tabs; they hold results already produced,
   exactly like after a connection switch today. Recorded rather than
   silently decided.
6. **Init always copies from the PROFILE**, never from an active workspace
   — consistent, because §W3 offers no folder picker in workspace mode (the
   route to a second workspace is profile-and-back).
7. **`ModalState::WorkspaceConfirm` is ONE variant with a
   `WorkspaceConfirmMode`** covering init / adopt / back-to-profile, so the
   gate, the „Aktivní připojení bude odpojeno." line and the Enter-inert
   policy exist once.
8. **dbc-mcp's broken-pointer refusal is scoped to the paths the command
   needs** (`--help` needs none, `setup --remove` needs none, `setup` needs
   the vault, `serve` needs both). §W7 said "exits with the error" without
   saying whether explicit overrides rescue it. **They do.** An
   unparseable argv (`Command::Usage`) is treated as needing BOTH defaults,
   because a typo'd `--confg` means config is NOT explicit and the broken
   pointer's default is exactly what would have been used.
9. **`context_switch_blocked` grew its dirty-script arm in Task 8**, not
   Task 5, because `script_binding` did not exist yet. One gate function
   throughout.
10. **`ScriptsListState::Loading` is a UNIT variant**, not the plan's
    `Loading { generation: u64 }`. The generation lives beside it on
    `SchemaTree::scripts_generation`; a copy inside the variant was pure
    redundancy and was never read.
11. **`ModalState::ScriptName` DOES carry a `mode` field**
    (`ScriptNameMode::{NewScript, NewFolder, Rename}`), contradicting the
    scripts-plan's deviation 5 — three modes do not collapse into
    `target_rel: Option<String>`, because create-file and create-folder
    share an empty `target_rel`.

## B. The shared fs rails, as they actually ended up

`dbc_state::fsutil` owns them (`join_component`, the
`conflicting_entry_ci` / `entry_exists_ci` probe, `write_atomic`), plus a
fourth added by the T10 sweep:

- **`fsutil::fold_name` is THE case fold** (`str::to_uppercase`, measured
  against NTFS's real `$UpCase` relation over 62,474 BMP names). T10
  carry-forward 6: three sites in two crates were folding names for the
  same question and did not agree — `conflicting_entry_ci` and
  `scripts::list_dir_sorted` used `to_uppercase`, while Task 9's
  `dbc_ui::path_fold` used `to_lowercase` while its own doc comment
  claimed to be applying `fsutil`'s rule. The disagreement was not
  cosmetic: `to_lowercase` implements Unicode's final-sigma context rule,
  so a folder `ΟΔΟΣ` and a folder `οδοσ` fold APART under it although NTFS
  resolves them to one directory — renaming or deleting that folder would
  have left the editor binding standing on a dead path, which is the exact
  bug `path_fold` was introduced to prevent. Unified onto `to_uppercase`
  and turned into a named function so a fourth site cannot quietly pick
  the other one. Measured, not argued (T9 re-verify FAIL-2, standalone
  `rustc` probe over the four interesting pairs):

  | Pair | `to_lowercase` | `to_uppercase` |
  |---|---|---|
  | `D:\ws\scripts\ΟΔΟΣ.sql` vs `…\οδοσ.sql` | true | true |
  | `D:\ws\ΟΔΟΣ\a.sql` vs `D:\ws\οδοσ\a.sql` | **FALSE** | true |
  | `D:\ws\scripts\Σ.sql` vs `…\ς.sql` | **FALSE** | true |
  | `D:\ws\straße.sql` vs `…\STRASSE.sql` | false | true |

  Row 2 is the killer, and it is the ANCESTOR arm of
  `script_binding_affected` / `path_starts_with_ci`. Row 1 explains why
  nobody noticed: `.sql` is Case_Ignorable-then-cased, so Final Sigma
  never fires on a FILE name — only on a DIRECTORY component. The five
  file-level sites looked fine and the ancestor site did not, and the
  suite discriminated nothing (the pre-existing `Řezy`/`řezy` pin folds
  identically under both folds; the reviewer flipped the one word and got
  948 passed / 0 failed). Now pinned by a directory-component sigma
  assertion in
  `the_binding_comparison_is_unicode_case_insensitive_like_every_other_probe`,
  mirroring `fsutil.rs`'s own `create_dir`-backed regression pin.
- **`.sql` extension tests are `dbc_ui::scripts::is_sql_path`**, one rail,
  four call sites, and it is `eq_ignore_ascii_case` on purpose: an
  extension is not a user-facing name and no Unicode extension exists.
- **`dbc_state::workspace::one_line_reason` is THE reason collapse.** T10
  carry-forward 5 moved it out of `dbc-mcp`, where it lived alone, into
  the crate that produces the `Resolution::Broken` reason — because the
  BLOCKING GUI modal (§W4, the one dialog the user cannot Esc out of) was
  rendering the raw multi-line `toml::de::Error` art. Both consumers apply
  it to the displayed PATH as well as to the reason, because the pointer's
  `path` field is arbitrary TOML text that nothing validates (T10
  carry-forward 3: a hand-edited `\n` in it otherwise bought a third
  stderr line of attacker-chosen text, falsifying `dbc-mcp`'s promoted
  "exactly two lines" property).

## C. Atomic writers — the honest count

The §W6.2 gate asked for exactly one tmp+`sync_all`+rename block. The
truth is **one NEW one plus four pre-existing ones**, and the fifth is
worth writing down rather than leaving as a surprise:

| Writer | Target | Status |
|---|---|---|
| `fsutil::write_atomic` | anything (scripts, marker, `.gitignore`) | THE phase's rail; `dbc-ui` reaches it only through `scripts::write_script`, pinned by `script_write_audit` |
| `grid.rs`'s CSV/JSON export | a path the user typed | pre-existing; **independently derives `<path>.tmp`**, byte-identical to `fsutil::tmp_path_for` |
| `er_diagram_view.rs`'s SVG export | a path the user typed | pre-existing; plain `fs::write`, no tmp |
| `AppConfig::save` | `config.toml` | pre-existing, explicitly sanctioned by the plan |
| `ParamStore::save` | `params.toml` | pre-existing, NOT migrated |
| `ViewPrefs::save` | `views.toml` | pre-existing, NOT migrated |
| `Vault::save` | `vault.bin` | pre-existing, NOT migrated; note it does tmp+rename with **no `sync_all`** |

The three store writers travel INTO the workspace folder in workspace
mode, so strictly they are "workspace-touching" and the plan's wording did
not anticipate them. They were deliberately left alone: migrating the
profile store layer is a `dbc-state` refactor, not a workspace-phase
change, and each writes one file nothing else writes. **The tmp NAMES
already agree** by accident of construction —
`path.with_extension("toml.tmp")` on `config.toml` and
`fsutil::tmp_path_for(config.toml)` both yield `config.toml.tmp` — so the
shipped `.gitignore`'s blanket `*.tmp` covers every one of them, which is
the property that actually mattered.

The two EXPORT writers (T9 re-verify NIT-A) reach folders the user chose,
but never the scripts library behind the user's back: both open
`prompt_for_new_path` with an EMPTY start directory, so the destination is
typed every time and no automatic path leads into the library. `grid.rs`'s
independent `<path>.tmp` derivation is nevertheless a second writer over a
convention whose single-writer contract `write_atomic`'s doc states as if
it were universal. Recorded, not fixed: it is pre-existing, it is only
reachable by exporting a grid onto the exact path of an in-flight script
save, and the blanket `*.tmp` covers it. The over-claiming sentence on
`script_write_audit` was narrowed to "into the scripts library" so the
doc no longer promises coverage it does not have.

Production `read_dir` in `dbc-state` is exactly TWO sites:
`fsutil::conflicting_entry_ci` (the probe) and `workspace::classify` (the
emptiness check, names only, never descends). `dbc-ui/src/scripts.rs` has
two more (`list_dir_sorted`, `delete_entry`'s emptiness check), both
outside `dbc-state` and both in scope for that crate's own rails.

**Final-review NIT-2 — the dbc-ui count is THREE, not two.** The
inventory above omitted `main.rs`'s `list_sql_files`, the pre-existing
G12 „spustit složku" pre-scan (one `read_dir`, non-recursive, reached
from the run-folder picker). It is not a scripts-library site — it lists
a folder the user typed into a dialog, and it is the only `read_dir` in
`dbc-ui` outside `scripts.rs` — but this branch DID touch it (T9 NIT-3
routed its extension test onto the shared `scripts::is_sql_path` rail),
so leaving it out made the count read as complete when it was not.

**Why it stays out of scope, stated rather than implied.** Unlike
`list_dir_sorted`, which filters with `entry.file_type()` and therefore
does NOT follow links, `list_sql_files` filters with `path.is_file()`,
which follows symlinks and NTFS junctions. A junction inside a picked
folder can therefore contribute a target the user did not see in that
folder. That is a pre-existing G12 behaviour on a path where the user
types the folder every time and nothing is written — the run is
read-only (`running_a_library_script_never_auto_saves_first`) — so it is
recorded, not changed: hardening it is a G12 decision about the run
feature, not a workspace-phase one, and doing it here would alter which
files an existing user's saved folder-run picks up.

## D. Git stays external — permanently

No `git2`, no `notify`, no `walkdir`, no `rfd` in any `Cargo.toml`. No
subprocess. **Nothing under `.git/` is opened, read, or parsed** — `.git`
appears in the source only as a name being SKIPPED (`classify`'s
dot-entry rule, `list_dir_sorted`'s dot-directory rule) and in comments
and tests. §W6.4 is permanent, not "not yet".

## E. Release-notes disclosures (§W7)

The repo keeps no separate release-notes file, so per Task 10 Step 6 they
live here:

1. **`vault.bin` cannot be merged by git.** It is an encrypted Argon2id
   envelope; a conflict is resolved by taking one side wholesale, and the
   losing side's newly-added passwords must be re-entered by hand. The app
   cannot and deliberately does not help. The shipped `.gitignore` carries
   a COMMENTED-OUT `vault.bin` line so excluding it is one uncomment away.
2. **Git history is permanent.** A `vault.bin` committed once cannot be
   reliably removed from history afterwards; the security of the whole
   repository then equals the strength of the master password. This is
   said in-app too (`WORKSPACE_GIT_WARNING`, §W6.3), at every decision
   point and never on startup.
3. **`tool_paths` travels with the workspace** and may name machine-A
   paths. Backup/restore then errors clearly until fixed in Nastavení.
4. **`history.sqlite` does NOT travel** — it stays machine-local by
   design (§W5) and is not among the files init copies.
5. **There is no app-exit dirty guard for a bound script.** No exit
   interception exists app-wide; this is the same posture as today's
   editor text, not a regression. A dirty binding DOES block a workspace
   swap and a script open (`context_switch_blocked`), just not app exit.
6. **A wedged scripts modal needs an app restart, and takes the unsaved
   buffer with it.** If a background fs op ever panicked, `running` would
   stay latched and the dialog would refuse Esc, „Zrušit", confirm and any
   workspace swap. Since T9 MAJOR-1 it also wedges **Ctrl+S**, because
   `script_save_allowed` refuses a save while any modal owns the screen —
   so the unsaved editor buffer cannot be saved at all before the process
   is killed. Accepted trade, argued in full on
   `connections_ui::script_modal_esc_closable`; the panic surface on that
   path is empty today (no `unwrap` / `expect` / `panic!` in `scripts.rs`'s
   production half or in the `fsutil` rails it calls), which is the
   assumption the whole trade rests on and must be re-checked if anything
   fallible is ever added to that path.

## F. What is NOT verified headlessly — the manual checklist

**Read this before believing the phase is done.** Everything below was
implemented and unit-tested where a unit test was possible, but was never
observed running. In particular **the „Skripty" sidebar section has never
been seen on screen**: it renders through GPUI, which has no headless
harness in this repo, so Tasks 7, 8 and 9's GUI passes are all still owed.
Collected here — numbered, ordered and reproducible — instead of scattered
across agent reports.

Build once: `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`.
Back up `%APPDATA%\dbc\` before starting; several items hand-edit it.

### F.1 Blocking „Pracovní prostor nenalezen" modal (T4 Step 8)

Hand-write `%APPDATA%\dbc\workspace.toml` and run TWICE:

1. `path = "D:\\neexistuje"` — the missing-folder case. Expect the modal
   titled „Pracovní prostor nenalezen", the path on its own line, and
   „error: složka neexistuje".
2. `path = "<%APPDATA%>\\dbc"` — the pointer aimed at the PROFILE DIR
   ITSELF (T4 review MAJOR-2). `certutil -hashfile %APPDATA%\dbc\config.toml`
   before and after must MATCH, and
   `%APPDATA%\dbc\__pracovni-prostor-nenalezen__` must NOT be created.
3. Per run: the connection dropdown behind the modal lists NOTHING, and
   the status bar reads „error: pracovní prostor nenalezen — vyberte
   složku, nebo použijte lokální profil" (NOT „ready").
4. Per run: Tab moves a visible focus ring across the three choices;
   Enter/Space activates the FOCUSED one; a bare Enter with the panel
   itself focused does nothing; Esc does nothing.
5. „Použít lokální profil" deletes the pointer, restores the real
   connection list, status „lokální profil obnoven", and
   `%APPDATA%\dbc\config.toml` is byte-identical to before the run.
6. **T10 addition.** Hand-write a pointer whose TOML does not parse (e.g.
   `path = "D:\ws"` — a lone `\w` is an invalid TOML escape) and confirm
   the modal's reason line is ONE line, not eight lines of `|`/`^` art
   echoing the pointer's own source text. (This is the carry-forward-5
   fix. `dbc_state::workspace::one_line_reason` is unit-tested; that it
   reaches the rendered panel is not.)

### F.2 Settings „Pracovní prostor" block (T5 Step 9)

7. Nastavení → „Použít složku…" → pick a NON-empty folder (e.g.
   `Documents`). Expect the refusal „error: složka není pracovní prostor
   dbc a není prázdná — …" and **not one file created** in it (`dir /a`
   before/after).
8. Pick a fresh EMPTY folder. The confirm modal shows the path, the copy
   line, „Aktivní připojení bude odpojeno." and the full §W6.3 warning →
   „Rozumím, vytvořit". Expect `dbc-workspace.toml`, `config.toml`,
   `vault.bin`, `views.toml`, `params.toml`, `scripts\` and `.gitignore`
   to exist; the profile files byte-identical (`certutil -hashfile`); the
   status „pracovní prostor: {path}"; the same connections in the
   dropdown; and the first connect re-prompting for the master password.
9. Nastavení → „Přejít na lokální profil" → „Přejít". Expect the
   workspace folder untouched, `%APPDATA%\dbc\workspace.toml` gone, status
   „lokální profil obnoven".
10. `findstr /S /I "<a real saved password>" <workspace>\*.toml <workspace>\.gitignore`
    finds NOTHING (the §W6.5 rail, by hand once).

### F.3 The „Skripty" sidebar, both arms (T7 Step 8) — NEVER SEEN

11. Profile mode with no `scripts_dir`: „Skripty" expands to „složka
    skriptů není nastavena — klikněte pro Nastavení"; the click opens
    Nastavení.
12. Pick a folder holding `a.sql` and `sub\b.sql`: the tree shows `sub`
    then `a.sql` (folders first); expanding `sub` shows `b.sql`; `⟳`
    re-scans after an external `copy` into the folder.
13. Switch to a workspace: the Settings block becomes the fixed
    „Skripty: {workspace}\scripts" line with NO picker, and the tree
    re-roots to the (empty) workspace `scripts\` — „žádné skripty (*.sql)".
14. Hand-edit `scripts_dir` into the WORKSPACE `config.toml` and restart:
    the tree still shows `<workspace>\scripts` (§W8 inertness).

### F.4 Editor binding (T8 Step 9) — NEVER SEEN

15. Double-click `a.sql` → the caption strip reads „Skript: a.sql", the
    editor holds the file, and NOTHING ran. (This is the whole
    double-click → `TreeEvent::ScriptOpen` path; nothing about it has been
    observed end to end.)
16. Type a character → the caption becomes „Skript: a.sql •" and „Uložit"
    brightens out of its dimmed-when-clean state. Ctrl+S → „skript
    uložen: a.sql", the „ •" disappears, and no `*.tmp` remains.
17. With a dirty buffer, double-click `b.sql` → the discard confirm names
    „Neuložené změny skriptu a.sql budou zahozeny."; „Zrušit" leaves
    `a.sql` bound and dirty; „Zahodit" opens `b.sql`.
18. „Zavřít" unbinds and the editor TEXT STAYS. Ctrl+S then opens save-as,
    defaulting into the library; saving `dotaz` writes `dotaz.sql` and the
    tree shows it.
19. With a dirty binding, Nastavení → „Přejít na lokální profil" refuses
    with „error: skript má neuložené změny — nejprve jej uložte nebo
    zavřete".
20. Open a >1 MiB `.sql` → „error: soubor je příliš velký pro editor
    (limit 1 MiB) — spusťte jej jako skript".
21. Ctrl+S while ANY dialog is open → „nejprve zavřete otevřený dialog",
    rendered in `warn`, not `danger` (T9 MAJOR-1; `.occlude()` blocks
    clicks, not keys, so this refusal is the only thing standing there).

### F.5 Create / rename / delete / run (T9) — THREE ITEMS ONLY

The T9 re-verify swept this task and confirmed the manual debt is
**exactly three items**: the two panels' layout, the focus arm, and
`running`-inertness rendering. Everything else about T9 — the fold, the
identity re-checks, the run-confirm disclosure, the notice colouring, the
delete/rename binding fixups, the pinned copy — is automated now. Do not
re-verify those by hand, and do not read their unit tests as a substitute
for the three below.

22. The two script modals' LAYOUT: `ScriptName` (create file / create
    folder / rename) and `ScriptDeleteConfirm`. Each renders a title, the
    text field or the target name, the notice slot, and two buttons.
23. The FOCUS ARM: opening `ScriptName` puts the caret in its text field,
    so typing works without a click.
24. `running`-INERTNESS RENDERING: while a background fs op is in flight
    both buttons and the mode radio must LOOK disabled (no
    `cursor_pointer`, no hover), matching `WorkspaceConfirm`. This is the
    only visual cue that Esc is being refused — see §E.6 for why a stuck
    latch is expensive.

### F.6 dbc-mcp out-of-process matrix (T6 Step 5) — THE ONLY PIN ON THE EXIT CODES

**This matrix is the only thing that pins `main`'s exit codes at all.**
`async fn main() -> ExitCode` is not unit-testable, and a reviewer's
mutation confirmed that changing the `Command::Usage` arm from
`ExitCode::FAILURE` to `ExitCode::SUCCESS` survives the entire `dbc-mcp`
suite green. **Re-run this matrix whenever `main`'s match arms change.**

> **RUN AND PASSED at v0.22.0 (T10, 2026-08-26)** — unlike the rest of §F,
> this section is a CLI and needs no GPUI, so the sweep executed it rather
> than merely writing it down. `%APPDATA%\dbc\workspace.toml` was created
> for the run and deleted afterwards; `%APPDATA%\dbc\config.toml` was
> `md5sum`-verified byte-identical before and after
> (`f5912efe4833765e20e1b89bde1410c1`). Results below, and three extra
> cases worth keeping:
>
> * **A broken pointer whose `path` contains a TOML `\n` escape** rendered
>   as `D:: eexistuje-t10-smoke` on ONE line — the carry-forward-3 fix
>   confirmed in the wild (it was reached by accident, writing `"D:\n…"`
>   into the pointer, which is exactly the hand-edit the fix is for).
> * **An UNPARSABLE pointer** (`path = "D:\wrong-escape"`) printed
>   `ukazatel na pracovní prostor je nečitelný (ukazatel na pracovní
>   prostor je poškozený: TOML parse error at line 1, column 12: missing
>   escaped value, expected …)` — two lines, the `|`/`^` art gone AND the
>   pointer's own source text not echoed. That is carry-forward 5 working
>   out of process; the GUI half (§F.1 item 6) is still owed.
> * **`--vault <real> --bogus` against a broken pointer** still diagnosed
>   the workspace, out of process. That is carry-forward 2 — the
>   `needs_config` arm nothing used to pin.
>
> Also confirmed: `--config <real> --vault <real>` against a broken
> pointer gets PAST workspace resolution entirely and fails for its own
> unrelated reason (no credential in the store), i.e. §W7's scope rule
> holds.

With `%APPDATA%\dbc\workspace.toml` pointing at a NON-EXISTENT folder:

29. `dbc-mcp --help 1>NUL` — exits **0**, stdout EMPTY, 27 lines of usage
    on stderr naming the pointer and `--config`/`--vault`. **PASSED.**
30. `dbc-mcp 1>NUL` — exits **1**, stdout **0 bytes**, and the Czech
    refusal on stderr as **exactly two lines**: the diagnosis and the
    escape hatch. A single byte of this on stdout would corrupt the
    JSON-RPC stream. **PASSED.**
31. `dbc-mcp --nonsense 1>NUL` — exits **1** (`Usage`), stdout 0 bytes,
    naming the bad argument AND diagnosing the workspace. **PASSED.**
32. `dbc-mcp --config <real> --vault <real>` against a broken pointer —
    gets past workspace resolution entirely. **PASSED** (it then failed on
    its own unrelated missing credential, which is the point).
33. With the pointer aimed at a REAL workspace: a bare run uses the
    workspace's `config.toml` and `vault.bin`, not the profile's.
    **STILL OWED** — needs a real initialized workspace, which needs the
    GUI.

### F.7 End-to-end multi-machine smoke (T10 Step 9)

34. Start in profile mode with real connections and a saved password.
    Settings → „Použít složku…" → fresh empty folder → „Rozumím, vytvořit".
35. `git init`, `git add -A`, `git commit` **in a terminal, outside the
    app**. Confirm the app noticed nothing and offers nothing git-related
    anywhere in its UI.
36. Add a script via `+`, save it with Ctrl+S, run it with `▶`.
37. Close the app; `git clone` the folder to a second path; restart;
    Settings → „Přejít na lokální profil", then „Použít složku…" → the
    CLONE → classified as adopt („Otevřít pracovní prostor") → the same
    connections, favourites, theme and scripts tree appear, and the first
    connect prompts for the SAME master password.
38. Rename the clone folder on disk while the app is closed; restart.
    Expect the blocking modal, Esc/Enter inert, an empty connection list
    behind it, and „Najít složku…" recovering it.

## G. Source audits — what they pin, and their exact counts

Three `#[cfg(test)]` audits read this crate's own source. **Each asserts
an exact count; changing one is a deliberate act, not a bump.**

| Audit | Needle(s) | Count | Scope |
|---|---|---|---|
| `config_save_guard_audit` | `.config.save(` | 6 | whole `src` tree |
| `editor_clobber_audit` | `replace_buffer`, `perform_script_action`, `bind_script` | per test | whole `src` tree |
| `script_write_audit` | `write_atomic` (1), `write_script` (5), `.save_script` (2), `script_save_allowed` (7) | as listed | whole `src` tree |

`script_write_audit` gained a FOURTH test in T10 (`the_writer_itself_is_reachable_only_through_the_guarded_entry_points`),
closing T9 re-verify FAIL-1. The `write_script` audit sanctions the OWNER
`save_script` unconditionally, so the chain stopped there and
`save_script`'s own callers were audited by nothing: the re-verify added a
plausible future handler calling `self.save_script(..)` directly, around
`script_save_allowed`, and got the whole suite green — the only signal a
dead-code warning that vanishes the moment it is wired to a listener. The
new test pins the writer's callers to `{on_save_script, save_script_as}`.
Its needle is `.save_script` **with the leading dot**, because `audit`
matches `needle + "("` as a plain substring and a bare `save_script` would
also swallow `on_save_script(`.

`script_save_allowed`'s count went 6 → 7 in the same fix: the predicate is
now asked TWICE in production. `on_save_script` asks it synchronously, and
`save_script_as` asks it AGAIN in its post-await continuation. That second
ask is the behavioural half of FAIL-1 — the file picker is not app-modal
on every platform, so the entry-point check is a statement about the past.
The concrete hole it closes: editor unbound and dirty → Ctrl+S → picker
opens → the user deletes `trzby.sql` from the tree and confirms (nothing
stops it: no write was dispatched, so `script_save_in_flight` is false;
and `was_bound == false`, so the binding generation is never bumped) →
the user completes the picker naming `trzby.sql` → the generation check
passes → the irreversibly deleted file is silently back. That is T9
MAJOR-1's own scenario, through the one entry point its fix did not
re-check.

T10 carry-forward 7 widened `config_save_guard_audit` onto
`editor_clobber_audit`'s machinery. It had been reading a HAND LIST of two
files with a prefix-match owner detector, so (a) the other 31 `.rs` files
in the crate were invisible to it although `AppView.config` is
crate-reachable from every one of them (`main.rs` is the crate root, so
every module is a descendant), and (b) a write inside an `async fn` /
`pub(super) fn` / `const fn` was attributed to the PREVIOUS function and
could be sanctioned by ITS guard call. It now walks `src/` at test time
and uses the exact-name `defined_fn_name` parser, with its own
non-vacuity test (`the_audit_reads_the_whole_crate_not_a_pair_of_files`).

The RULE deliberately still differs from the other two: this audit asks
that the guard be CALLED in the same function above the write, whatever
that function is named, because the guard is cheap, idempotent and correct
to call unconditionally — a new writer should add the call rather than add
its name to a list. The other two sanction by owner NAME.

`AppView::guard_corrupt_config` is additionally `#[must_use]` (T10
carry-forward 1). The audit only proves the call is THERE; a reviewer's
mutation showed that `self.guard_corrupt_config(cx);` — verdict called and
discarded — passes the audit while doing exactly what the guard exists to
prevent (the `false` return IS the abort signal). The attribute makes the
bare call a warning, i.e. a build failure under this repo's zero-warning
gate; verified by mutation during T10.

## H. Deliberately declined by the T10 sweep

- **The stuttering broken-pointer subject.** With `root: None` both the
  MCP stderr line and the GUI modal read „…: ukazatel na pracovní prostor
  je nečitelný (ukazatel na pracovní prostor je poškozený: …)", because
  the subject (`WORKSPACE_MISSING_NO_PATH`, byte-pinned in two crates) and
  the predicate (`read_pointer`'s reason) both name the pointer. Left
  alone: both halves are verbatim from binding sources pinned by different
  tasks, and quietly harmonising copy across a seam is the trap this phase
  has recorded twice already. Reword BOTH crates together, or neither.
- **Migrating `ParamStore` / `ViewPrefs` / `Vault` onto `write_atomic`** —
  see §C. A `dbc-state` store-layer refactor, not a workspace-phase change.
- **A real failure path for a panicked background fs op** — see §E.6 and
  the argument on `script_modal_esc_closable`. The alternative (a timeout,
  or an Esc-anyway hatch) reintroduces a second writer on
  `write_atomic`'s fixed `<path>.tmp`, which is silent data loss instead
  of a visible wedge.

# Jak to nakonec je (as-built) — final-review pass, v0.22.0

Written after the FINAL WHOLE-BRANCH REVIEW of the workspace-folder +
scripts-library phase. Everything in §A–§H above still stands; this
section records what that review changed and why. Where it and §A–§H
disagree, this section wins.

## I. The data-loss fix (MAJOR-1)

`confirm_script_delete` computed `was_bound = binding_targets(rel, is_dir)`
BEFORE dispatching the background delete, and `finish_script_delete`
applied that boolean blind. This is the phase's own banned shape — *a
check performed before an `await` is a statement about the past* — and it
lost data in one direction:

1. Editor UNBOUND. Double-click `trzby.sql` → `read_script` dispatched.
2. Right-click → Smazat → confirm. `was_bound == false`. Delete dispatched.
3. The READ lands first. `script_open_abort_reason` passes all three legs
   — the root did not move, the buffer was not typed into, and the
   generation did not change *because an unbound editor never called
   `set_script_binding`, so nothing bumped it* — and `bind_script` binds
   the doomed file.
4. The delete lands. `was_bound == false`, so the binding is NOT cleared;
   the caption still names a file that no longer exists.
5. Ctrl+S — `script_save_allowed` passes, the modal is long closed — and
   the irreversibly deleted file is silently back on disk.

The symmetric direction was milder and equally wrong: bound to `a.sql`,
an in-flight open of `b.sql` landing during a confirmed delete of `a.sql`
dropped the brand-new `b.sql` binding, so the next Ctrl+S silently became
a save-as.

**Fixed by re-ASKING at the landing.** The question is now the free fn
`binding_targets_entry(binding, root, rel, is_dir)`, so no parameter can
carry a dispatch-time answer; `binding_targets` is a thin wrapper; `is_dir`
travels instead of the bool, because it describes the deleted entry, which
cannot change while the op runs. `retarget_binding_after_rename` — the
delete's sibling — was already written this way, which is why rename never
had this bug. Both directions pinned, plus the no-binding / no-root /
empty-rel arms.

The delete confirm's second line is now explicitly a WARNING about the
moment the user is looking at it, not the decision. The decision is made
when the delete lands.

## J. The audits stopped being the primary rail (MAJOR-2)

The reviewer defeated two of the four source audits on a LIVE production
path (`perform_script_action`'s `Unbind` arm) with **zero warnings and
11/11 audits passing** — reproduced independently on `93b7d87` during this
pass, 950 passed / 0 failed. Root cause both times: *the audit pinned the
mention, not every path to the thing.*

- `config_save_guard_audit` keyed on the literal `.config.save(`, so
  `let cfg = &self.config; cfg.save(&self.config_path);` was invisible.
- `script_write_audit` keyed on `.save_script(` **with the leading dot**,
  and its doc comment argued at length for the dot. UFCS puts a colon
  there: `AppView::save_script(self, …)` contains `::save_script(`.

That is five separate defeats of a text audit across this phase, this one
by the most ordinary alternative call syntax in Rust. So the rule moved
into the type system, and the audits became the belt rather than the
braces.

**`save_guard::SaveAllowed`** — a witness with a private tuple field,
demanded by value by `AppView::save_script`. Rust's finest privacy
granularity is the module and `main.rs` is the CRATE ROOT, so a private
field declared there is visible crate-wide and proves nothing; the witness
therefore lives in a CHILD module, because a parent cannot see a child's
private items. That asymmetry is the whole mechanism. The only mint is
`save_guard::save_allowed_now(&AppView)`, which reads the three facts off
the live view — a three-boolean mint would let a caller pass whichever
three suited it. `SaveAllowed` is deliberately neither `Copy` nor `Clone`,
so `on_save_script`'s witness cannot be carried across `save_script_as`'s
file picker even on purpose: the „check before an await" rule, made
structural.

**`dbc_state::ConfigSaveGuard`** — demanded by `AppConfig::save`. A
cross-crate witness cannot have a private constructor (the minting code
lives in the other crate), so this one proves the PRECONDITION instead of
proving that a function was called: the only mint is
`AppConfig::verify_savable(path)`, which re-reads the file and refuses
when it does not parse. That is stronger, because it holds however the
writer is reached. Six `dbc-ui` call sites threaded, plus `dbc-state`'s
own.

Consequence worth recording: **`guard_corrupt_config` stopped trusting
`config_load_error` for the decision.** That flag is set at STARTUP, and
the phase's own rule applies to it too — a `config.toml` corrupted by an
external editor *after* launch used to be overwritten with no backup.
The flag is still cleared (so the startup banner stops nagging), but the
question „will this save destroy something" is now asked of the disk,
every time. Cost: one small read per save, on a user gesture.

The sentinel-folder comment's over-claim was corrected in the same spirit
(MINOR-3): „every store open and every save against it fails loudly" was
half false — all four store savers `create_dir_all` their own parent, so a
stray save SUCCEEDS and leaves debris. The comment was corrected rather
than the behaviour, because the alternatives are a platform-specific
unusable name (`NUL` fails on Windows and succeeds on Linux — a rail that
silently stops working on one target is worse than an honest comment) or
stripping `create_dir_all` from four pre-existing savers, which is what
makes a first run work and is the §C refactor already declined. The
security invariant — never the profile's real files — is structural and
unaffected.

### The scanner, three structural gaps

- **`sources()` walked `dbc-ui/src` only.** `crates/dbc-ui/tests`, a
  future `build.rs` and every other crate were invisible — notably
  `dbc-state`'s FOUR `write_atomic` callers (`workspace.rs:265, 394, 433,
  445`), which write real bytes into the user's folder and were audited by
  nothing while `the_shared_atomic_writer_has_exactly_one_funnel` asserted
  "exactly one caller". It now walks every workspace member's
  `src`/`tests`/`build.rs` — 79 files, up from 33 — and that audit was
  re-audited honestly: 10 sites, 5 production owners named individually.
- **`code_of()` truncated at the first `//` anywhere**, including inside a
  string literal, where it swallowed the rest of a real statement (a
  hiding place, not merely a false positive: put a URL first and the call
  vanished); block comments were never stripped at all. Replaced by
  `code_lines()`, a scanner over nested multi-line block comments, `"…"`
  with escapes, `r"…"`/`r#"…"#` raw strings, and char literals told apart
  from lifetimes (`'/'` and `'"'` are both real code here).
- **`owner_fn()` did no brace balancing**, so a call at file scope after a
  sanctioned function CLOSED inherited its sanction. `owners()` tracks
  depth; closures do not steal ownership, which is the answer these audits
  want.

Both broken needles are receiver-independent now: `save_script(` through
the new `audit_excluding` (naming `on_save_script` as the false positive
instead of dodging it with punctuation — the punctuation *was* the
bypass), and the config write keyed on the ARGUMENT `config_path` rather
than on any receiver.

### The injection, re-run

Verified by mutation in both directions rather than by argument:

| Tree | Injection | Build | Audits |
|---|---|---|---|
| `93b7d87` (pre-fix) | reviewer's exact two | zero warnings | **950 passed, 0 failed — 11/11 green** |
| fixed | reviewer's exact two | **2 compile errors** (`&ConfigSaveGuard` missing, `SaveAllowed` missing) | not reached |
| fixed | same, but minting both witnesses legitimately | zero warnings | **3 FAILED** — see the correction below |

Re-verify NIT-3 corrects that last cell: only TWO of the three named the
injected line. The third failed on a COUNT assertion, because the
injected block happened to mention `guard_corrupt_config` and thereby
satisfied `config_save_guard_audit`'s „the guard is mentioned above the
write" rule. A mention-based rule is exactly what keeps getting beaten,
so reporting it as a clean catch hid the thing worth knowing.

The third row is why the text audits were kept: a caller that satisfies
the type rail honestly and still writes from the wrong place is caught by
`every_config_toml_write_passes_the_corrupt_config_guard`,
`the_writer_itself_is_reachable_only_through_the_guarded_entry_points`
and `the_save_witness_is_minted_at_every_stoppable_point_and_nowhere_else`.
Reverted afterwards; `git diff --quiet` clean.

## K. The recovery modal is an ADOPT, and now says so (MINOR-1)

`pick_workspace_for_recovery` accepted any folder with a valid
`dbc-workspace.toml`, wrote the pointer and called `apply_context` the
instant the folder dialog returned. Settings' adopt has always shown
„Trezor tohoto prostoru se odemyká jeho vlastním master heslem." and
`WORKSPACE_GIT_WARNING` first (§W3.3/§W6.3, *"renders STATICALLY wherever
the folder-pick flow is offered"*); recovery showed neither, so the user
took on a foreign encrypted vault and a versioned-secrets decision
uninformed.

The confirm is a **SECOND STATE of the blocking modal**
(`WorkspaceMissing.pending`), NOT a `ModalState::WorkspaceConfirm`.
`WorkspaceMissing` is the one modal with no Esc and no close path, and
handing the screen to an Esc-closable confirm would let the user cancel
their way into an app with no context and no dialog — strictly worse than
the bug being fixed. „Zpět" returns to the three choices; nothing closes
this modal but a committed decision.

The copy is not duplicated: the panel renders
`workspace_confirm_lines(WorkspaceConfirmMode::Adopt)`, the same vector
Settings renders, so the two adopt paths cannot drift apart.

Side effect worth having: the pick now persists NOTHING at all (a
strengthening of T4 review MAJOR-1's guard, which only promised "nothing
until the UI-thread guard passes"), and the one write left is synchronous
with no await near it, so it needs no re-verification.

**The startup three-choice panel stays warning-free** (§W6.3: never on
startup — the user has not chosen a folder at that screen) and
back-to-profile is untouched. Both are negatively pinned.

## L. Smaller corrections

- **MINOR-2 — a blocked start's paths are now always ABSOLUTE.** A
  hand-edited `path = ""` walked past every guard in `blocked_paths`
  (`"" != profile`, and `Path::new("").canonicalize()` errors on Windows
  so `is_same_dir` said false), leaving `base = ""`; `workspace_paths("")`
  then yields the bare RELATIVE names `config.toml`, `vault.bin`, …,
  resolved by the OS against the process CWD — which, if the app was
  launched from `%APPDATA%\dbc`, are the profile's real files. The rule is
  now absolute-or-sentinel, in the testable `blocked_base`. Deliberately
  STRICTER than the review's "relative and non-canonicalizable": `..`
  canonicalizes fine and still resolves against a CWD this app does not
  set. `write_pointer` already refuses to WRITE a relative root for this
  exact reason; this is the matching refusal on the way in, where a
  hand-edited pointer never passed that rail.
- **NIT-1 — `save_script_as` re-checks the captured buffer too.**
  `open_script` re-asks root + generation + text; save-as asked only the
  first two, and the generation is structurally blind to typing. The
  picker is not app-modal on every platform, so keystrokes during it were
  invisible and the file got the pre-picker text with `saved_text` bound
  to text the user could no longer see. It now refuses, in the open path's
  own words.
- **NIT-3 — the last unfolded path comparison.**
  `script_open_abort_reason` compared roots with an exact `!=` on
  `Option<&Path>` while every other comparison goes through `same_path_ci`
  — the exact shape T10 carry-forward 6 existed to eliminate.
- **NIT-4 — `dbc-mcp`'s `-h`** was accepted since the first draft and
  documented nowhere. Now in the usage text, and every accepted flag is
  asserted present there so the next one cannot go undocumented either.
- **NIT-2 — the §C `read_dir` inventory** was completed; see the note
  added there.

## M. Confirmed correct, do not "improve"

Re-verified during this pass and deliberately left alone: the three rails
of `editor_clobber_audit`; `confirm_workspace`'s success arm having no
post-await check (sound, not lucky — the modal is latched while
`running`); `fsutil::fold_name` being `to_uppercase` with the sigma pin on
a DIRECTORY component; `history.sqlite` not travelling; git staying
permanently external (no dep, no subprocess, nothing under `.git/` ever
opened). The two release-only `chart_data` failures
(`prepare_ragged_y_column_trips_debug_assert`,
`scale_to_non_finite_value_trips_debug_assert`) remain the only two, and
remain pre-existing backlog.

# Jak to nakonec je (as-built) — scoped re-verify pass, v0.22.0

Written after the SCOPED RE-VERIFY of §I–§M returned FAIL. Sections I–M
stand except where corrected below; where they and this section disagree,
this wins.

The re-verifier reproduced all three rows of §J's injection table and
confirmed MAJOR-1, MINOR-1/2/3 and every NIT — and then found **four
fresh bypasses that compiled with zero warnings and passed 961/961
including all 11 audits**, one live data-loss path, and one claim in §J
that was false about the shipped code. A fifth bypass was found by this
pass while deliberately hunting for one.

## N. The false claim, and the live bug behind it

**§J said `SaveAllowed` could not cross `save_script_as`'s picker. It
could.** The claim rested on `!Copy + !Clone`, which forbid a second USE
of one value and say nothing about a MOVE. `SaveAllowed` was `Send +
'static`, so `async move` captured it happily: give `save_script_as` the
witness as a parameter, drop the re-mint, and T9 re-verify FAIL-1 is back
whole — Ctrl+S → picker opens → the user deletes `trzby.sql` and confirms
→ the picker completes naming `trzby.sql` → the irreversibly deleted file
is silently on disk again. Clean build, 961 green. The only thing catching
the honest version of that refactor was the text audit's site count — the
belt, not the braces §J credited.

**Fixed by making the permission a SCOPE rather than a value.**
`save_guard::with_save_permission` hands `SaveAllowed<'brand>` to a
`for<'brand> FnOnce(..) -> R` closure. `'brand` is generative and
invariant, and `R` is one type chosen before `'brand` exists, so the
witness cannot be returned, stored, or captured by `cx.spawn`'s `'static`
future; the closure is synchronous, so there is no await inside to hold it
across. Re-running the re-verifier's exact refactor now yields E0521,
*borrowed data escapes outside of method*.

The doc comment now claims only what the type delivers: **the permission
cannot leave the synchronous scope in which the predicate ran.** Nothing
about awaits in general, nothing about `!Clone`.

**MINOR-A — MAJOR-1's resurrection survived in the mirrored ordering, and
that was live data loss.** §I fixed „the open lands, then the delete
lands". The mirror: with the editor UNBOUND at the landing, the re-asked
`binding_targets` is false, so `set_script_binding` is never called and
`script_binding_generation` is never bumped — so an `open_script`
dispatched BEFORE the delete lands AFTER it, passes all three legs of
`script_open_abort_reason`, and binds the file that was just irreversibly
deleted. The next Ctrl+S recreates it. Windows opens the read with
`FILE_SHARE_DELETE`, so it completes across the delete and raises no error
to notice. `finish_script_delete` now calls
`supersede_script_continuations()` **unconditionally** — the lesson
`apply_context` had already learned and written down. A conditional bump is
missing precisely in the case where nothing local looks wrong.

## O. Text audits die to the next call syntax — seven times now

§J said the audits were „the belt" and named two type rails as the braces.
That was right about `save_script` and the config write and wrong about
everything else, and the re-verifier proved it in one line:

```rust
use crate::scripts::write_script as persist_bytes;
let _ = persist_bytes(&doomed, "-- truncated by the run");
let clobber = crate::sql_input::SqlInput::replace_buffer;
self.sql.update(cx, |s, cx| clobber(s, "", cx));
```

The needle was `ident + "("`, which still assumes a call syntax; an alias
binding puts a space or a `;` after the identifier and the call site never
names it. Zero warnings, 961 green, a truncating write into the library and
an unguarded editor clobber.

**The audits no longer look for a CALL. They look for the NAME.** An alias
has to be *introduced* somewhere, and introducing it writes the identifier
down — `use … as`, a fn-pointer binding, a re-export, a qualified path, a
`macro_rules!` body, all of them. So a whole-word mention of the identifier
anywhere in code is a site and must sit inside a sanctioned function. There
is no call syntax left to vary. A plain `use` (and a re-export) is
bookkeeping rather than a site, because it carries the name forward so every
call through it still spells it; a RENAME inside a `use`, group included, is
a site. Whole-word boundaries are what keep `on_save_script` from matching
`save_script` — the job the deleted leading dot used to do.

### The scanner's own two holes

**`code_lines` mis-parsed byte raw strings and HID real code.** The raw
branch required the character before the `r` not be identifier-ish, so the
`b` of `br#"…"#` failed the test and the literal fell into the ordinary-`"`
branch, which took the first quote in the payload as its terminator. With an
odd number of quotes the scanner desynced into „inside a string" and blanked
every subsequent character **across lines, to end of file** — hiding a
`write_script(` call from every audit, and mid-file silently blanking
*legitimate* calls so counts dropped (`replace_buffer` 2→1, `bind_script`
1→0). Live `br#"…"#` literals already existed at
`dbc-driver-postgres/src/types.rs:201,208,227`; they survived only because
their quote counts happen to be even. The prefix is now parsed properly —
`b`, `c`, `r`, `br`, `cr` — with the not-identifier-ish test applied to the
START of the prefix, which is the only place it was ever meaningful. §J's
claim that `br"…"` was handled was false and is corrected.

**`sources()` enumerated `crates/<crate>/{src,tests,build.rs}`**, so
`#[path = "../gen/evil.rs"] mod evil;` was invisible — as were
`crates/dbc-buffer/benches/` (which exists), `examples/`, generated trees,
and any member outside `crates/`. It now walks the workspace ROOT, skipping
only build output and VCS metadata, and the coverage test derives its member
list from the root `Cargo.toml` instead of hard-coding ten crate names — a
list that could not notice a new member, which was half of why the bypass
worked.

### The fifth bypass, found rather than reported

Widening the walk does not answer the same trick aimed OUTSIDE the root:

```rust
#[path = "../../../../outside-tree/evil.rs"]
mod evil;
```

Verified before fixing: it compiles, and 963 tests pass with a truncating
`write_script` and an unguarded `replace_buffer` in a file no walk can
reach. No amount of widening fixes it, because the file is not in the tree.

So the rail is aimed at the **escape hatch**. Two constructs move CODE from
an arbitrary path into this workspace and neither is used anywhere in it:
`#[path = …]` on a `mod` (any spelling, `cfg_attr` included) and
`include!(…)`, which splices tokens. Both are banned workspace-wide.
`include_str!` / `include_bytes!` are deliberately not banned — they carry
data, cannot introduce a call site, and this crate uses `include_str!` for
its own source pins. Banning the class beats chasing the instance.

**Correction (second re-verify, FAIL-7):** as first written that ban was a
SPELLING TEST, not a ban — three ordinary spellings walked past it. See
§U. It is a real check now, and §T states what it is worth.

A `macro_rules!` wrapper was also tried and is caught by the existing rails:
the macro body still names the identifier.

## P. What is compiler-enforced, and what is not

Stated plainly, because §J blurred it and that is how the round passed.

**Compiler-enforced (two):**
- The Ctrl+S permission — `save_guard::with_save_permission`, a
  generative-brand scope in front of `AppView::save_script`.
- The config write — `dbc_state::ConfigSaveGuard`, mintable only by a real
  parse of the very file about to be overwritten, and (re-verify NIT-1) now
  carrying that path so `verify_savable(a)` + `save(b, &g)` no longer
  type-checks.

Both are backed by `#![forbid(unsafe_code)]` on `dbc-ui` (re-verify
MINOR-C): every witness rests on a private constructor, and
`unsafe { std::mem::zeroed() }` forges any of them in one line with no
warning. `forbid`, not `deny`, because `deny` is undone by an `#[allow]` on
the offending item. The crate contains no `unsafe` today, so it costs
nothing.

**Audit-only (four): `write_script`, `write_atomic`, `replace_buffer`,
`bind_script`.** The re-verifier asked for witnesses on these. Declined,
with reasons:

- A witness is only as strong as the check its MINT performs. Rust privacy
  is module-scoped and `main.rs` is the crate root, so a token in a child
  module is unspellable elsewhere — but its mint function is callable from
  anywhere, and an unconditional mint is theatre: the attacker calls the
  mint and proceeds. That is precisely why `with_save_permission` works — it
  is a scope that RUNS the predicate — and aliasing it buys nothing.
- For these four there is no precondition to run. „May this code write into
  the library" is answered upstream by the Ctrl+S guard and by
  `create_script`'s collision probe; a token on `write_script` minted by
  `fn permit() -> Permit { Permit(()) }` would add a type and no guarantee.
- `fsutil::write_atomic`: `dbc-state` cannot run `dbc-ui`'s precondition,
  and `write_atomic` has none of its own — its four `dbc-state` callers are
  legitimate writers of one well-known file each, sharing nothing a mint
  could check.

  **Correction (third re-verify).** The sentence here previously read
  „cross-crate, so a private constructor is impossible on the minting
  side". That is literally false, and falsified by this very phase:
  `dbc_state::ConfigSaveGuard` is a cross-crate witness with a private
  constructor, in that same crate. The argument above is the one that was
  meant, and it is the one that holds.
- `replace_buffer` — **this decline was WRONG and has been reversed;
  see §S.** The claim that „the codebase has already declined this twice
  in writing" was also false: there is exactly ONE prior decline
  (`editor_clobber_audit`'s doc, from Task 8, unchanged across the whole
  history of `main.rs`). The „second" was this sentence, added in the same
  pass that cited it. A doc citing itself as corroboration is how a weak
  decision becomes load-bearing, and it is recorded here rather than
  quietly deleted because that is the failure mode worth remembering.

So the honest statement, which the doc comment on `script_write_audit`
also makes: the three remaining declines are held up by **source-text**
audits that pin every mention of the identifier and its count, over the
files the walk can see. §T sets out what those audits can and cannot
promise, and it is less than this section originally implied.

## Q. Smaller corrections in this pass

- **MINOR-B — a source pin was vacuous.**
  `the_save_as_continuation_re_asks_all_three_things_it_captured` asserted
  Leg 2 against the RAW body, where `script_save_allowed` occurs only in a
  COMMENT; the code calls the mint. That is the prose-satisfies-the-
  assertion failure `config_save_guard_audit`'s own doc warns about,
  reproduced two hundred lines from the warning. Every leg now asserts
  against `code_lines` output, and the old needle is asserted ABSENT from
  the code — both the real invariant and a standing proof the old version
  proved nothing.
- **NIT-2 — `guard_corrupt_config` treated any refusal as corruption.**
  `AppConfig::verify_config` now returns a three-way `ConfigVerdict` in ONE
  read. Only `Unparsable` may be moved aside; `Unreadable` (a file lock, an
  antivirus scan, a share blinking) refuses the save and says so. Before
  this, a transient read failure renamed a perfectly good `config.toml` to
  `.corrupt-bak` and told the user it had been corrupt.
- **NIT-3 — §J's injection table said row 3 was „3 audits FAILED", and
  that was imprecise in a way that matters.** Only TWO named the injected
  line. The third failed on a COUNT assertion, because the injected block
  happened to mention `guard_corrupt_config` and thereby satisfied
  `config_save_guard_audit`'s „the guard is mentioned above the write"
  rule. A mention-based rule is exactly what keeps getting beaten, and
  reporting it as a clean catch hid that.

## R. The bypass table, re-run

| Bypass | Before | After |
|---|---|---|
| Reviewer's exact two (UFCS + receiver rebind) | zero warnings, 950 green on `93b7d87` | **2 compile errors** |
| FAIL-1 alias / fn-pointer | zero warnings, 961 green | audit FAILS, names `main.rs:7116`; fn-pointer trips the count |
| FAIL-2 carried witness across the picker | zero warnings, 961 green | **E0521**, borrowed data escapes |
| FAIL-3 odd-quote `br#"…"#` hiding a write | zero warnings, 961 green | audit FAILS, names the hidden call |
| FAIL-4 `#[path]` inside the crate dir | zero warnings, 961 green | audit FAILS, names `crates/dbc-ui/gen/evil.rs:5` and `:6` |
| MINOR-C `unsafe { zeroed() }` forge | (not tried) | **compile error**, `forbid(unsafe_code)` |
| FIFTH: `#[path]` OUTSIDE the root | zero warnings, 963 green | audit FAILS, names the attribute line |
| `macro_rules!` wrapper | (not tried) | audit FAILS, the macro body names it |

Every row was run in this worktree, reverted afterwards, and the tree
confirmed clean.

# Jak to nakonec je (as-built) — second scoped re-verify, v0.22.0

Written after the second scoped re-verify of §N–§R returned FAIL. Those
sections stand except where corrected here and in §J/§P, which this pass
edited in place.

The generative brand survived a serious attack — twelve shapes, including
a `RefCell` stash, a `thread_local`, `Box<dyn Any>`,
`Box<dyn FnOnce + 'static>`, a `cx.spawn` capture, returning it as `R`, an
fn *item* taking `SaveAllowed<'static>`, explicit variance both ways,
forging from the crate root, and `#[allow(unsafe_code)]` + `mem::zeroed`.
It held every time. Everything else in that round needed work.

## S. `replace_buffer` — the decline that was wrong

§P declined a witness on `SqlInput::replace_buffer`, citing the Task 8
note: a real rail would mean moving `AppView.sql` behind a private
accessor, splitting `impl AppView` across files, and dragging the
autocomplete plumbing with it.

**That is a fair objection to the shape Task 8 proposed and irrelevant to
a scope.** No accessor, no module move: the editor entity stays exactly
where it is. What changed is that `replace_buffer` does not compile
without a `BufferReplace<'brand>` that only `editor_guard` can mint.

The precondition is real — which is the whole test for whether a witness
is a rail or theatre. The editor holds nothing unsaved, **or** the user
has just answered „Zahodit" for the action being performed.
`editor_load_guarded` already computed exactly that; now nobody else can
decide it.

The second arm is a fact about the PAST that no later read recovers, so
`on_discard_confirm_yes` records it in `AppView::editor_discard_grant` —
**stamped with the `script_binding_generation` it was granted at, and
consumed once**. Every path that moves the binding bumps that generation,
so a stale grant expires on its own instead of waiting to be spent; the
same reasoning `script_open_abort_reason` applies to the read it guards.
The grant's writers are name-audited (4 mentions, 3 owners) — a small
surface for a name audit, and the smallest this one could be given.

Verified: a `replace_buffer` call injected into `perform_script_action` —
an explicitly SANCTIONED owner, where every text audit passes — is now a
**compile error**. This closes the clobber half of FAIL-6, FAIL-7 and the
round-2 fn-pointer bypass structurally rather than by inspection.

## T. What the audits can and cannot promise

§P blurred this and §O overstated it. Precisely:

**Compiler-enforced (three now):**

| Rail | Precondition it runs |
|---|---|
| `save_guard::with_save_permission` → `AppView::save_script` | no dialog owns the screen |
| `editor_guard::with_editor_replaceable` → `SqlInput::replace_buffer` | editor not dirty, or the user just discarded |
| `dbc_state::ConfigSaveGuard` → `AppConfig::save` | the file about to be overwritten parses |

All three are backed by `#![forbid(unsafe_code)]` — on `dbc-ui` since the
last round, and now on `dbc-state` too, because `ConfigSaveGuard` is
DEFINED there and `mem::zeroed` could forge it there.

**Audit-only (three): `write_script`, `write_atomic`, `bind_script`.**
The re-verifier accepted these declines, and this round found the reason
the write ones cannot be fixed the way the editor one was — by running it
rather than asserting it:

> Giving `scripts::write_script` a brand-bound permit does not compile.
> The write is dispatched into `cx.spawn` / `background_spawn`, whose
> future must be `'static`, and the compiler says
> **„lifetime may not live long enough"** at the spawn. A permit that
> COULD cross that boundary would have to be `'static` — and a `'static`
> permit is exactly as leakable as the fn pointer it would replace.

That is the structural difference between the two writers: `replace_buffer`
runs synchronously on the UI thread, so its permit can be brand-bound;
`write_script` runs on a background thread, so its permit cannot be.
`bind_script`'s precondition is `open_script`'s and is enforced upstream.
`write_atomic` has no precondition of its own, and `dbc-state` cannot run
`dbc-ui`'s. (§P previously said a cross-crate private constructor was
impossible — false, and falsified by `ConfigSaveGuard` in that same crate.
Corrected in place.)

**The ninth bypass, found by this pass, and NOT closed.** A closure
wrapper at a sanctioned site:

```rust
// inside `save_script` — a sanctioned owner, mention count unchanged
let w = |p: &Path, t: &str| crate::scripts::write_script(p, t);
PROBE.with(|c| c.set(Some(w as fn(&Path, &str) -> Result<(), String>)));
w(&job_path, &job_text)
```

0 warnings, 966 passing. The mention IS a call, so re-verify FAIL-8's rule
is satisfied; the owner IS sanctioned; the count is unchanged. The
capability escapes inside the closure, which names nothing.

The same trick against `replace_buffer` **fails to compile** (`E0521`,
borrowed data escapes outside of closure), which is the clearest possible
statement of what a type rail buys over an audit.

It is recorded rather than patched because no source-text rule closes it:
a closure body is legitimate code at a legitimate site, and any heuristic
that flagged it would flag the real call too. What it means in practice:
**these audits defend against an accidental new writer and against
refactors that detach a name — they are not a defence against a
deliberate leak from inside a sanctioned function.** Anyone who can edit
`save_script` can also just call the writer. The type rails are what stop
even that, and there are three.

## U. The predicates the scanner got wrong

Four, each beaten by an ordinary spelling rather than a trick.

- **FAIL-9 was RED on a fresh checkout of this branch** — not theoretical.
  `code_lines` split on the newline and KEPT the carriage return, so on a
  CRLF checkout every logical line ended in an invisible `\r`, and the §N
  pin compares one with `assert_eq!`. This machine has
  `core.autocrlf = true` globally, so a fresh worktree gave
  **963 passed / 1 failed** and the release run **961 / 3** — the two known
  `chart_data` failures plus this one — while the live worktree passed
  because its `main.rs` had been written by an editor as LF. §J's counts
  and its „exactly the two known failures" were therefore false as
  delivered. `code_lines` now drops `\r`; every consumer wants logical
  lines and none wants the terminator. **Lesson recorded: a worktree whose
  files were written rather than checked out cannot reproduce this class
  of bug, so the gates are now run in a freshly checked-out tree.**
- **FAIL-6 — `sources()` pruned by PREFIX** (`starts_with("target")`), so a
  plain `mod targets;` was invisible to every audit, as were
  `target_picker/` and `targeting/`. No trick, and `targets` is a name
  somebody could add innocently. Nothing is pruned by name shape now:
  metadata by EXACT name, build output by cargo's own `CACHEDIR.TAG`
  marker. A directory a developer names is always scanned.
- **FAIL-7 — the `#[path]` / `include!` ban was a spelling test.** It asked
  whether ONE LINE held both `#[` and `path =`. Three spellings walked
  past: no spaces around `=`, the attribute split over two lines, and
  `include!{…}` with a brace. A ban a formatter can defeat is not a ban.
  The file's code is now flattened with all whitespace removed before
  matching, which makes spacing and line breaks irrelevant by
  construction, with a map back to real line numbers for the report.
  `cfg_attr` is banned whole (unused anywhere here); `include_str!` and
  `include_bytes!` stay legal because they carry data, not call sites.
- **FAIL-8 — the name rule bounds the identifier, not the capability.**
  Rewriting `save_script`'s single existing mention — sanctioned owner,
  count unchanged — as `let w = crate::scripts::write_script;` leaked the
  writer as a fn pointer. Every mention must now be a CALL or a plain
  import; a binding, a rename, or passing it as an argument is flagged
  where it happens. (This is what the ninth bypass then goes around, by
  making the mention a real call inside a closure.)

## V. Smaller corrections

- `AppConfig::save`'s exact-bytes path compare is deliberate and now says
  why: `same_path_ci` answers „do these two names reach the same file on
  disk", a filesystem question needing the Unicode fold; this asks „is
  this the same value the caller proved something about", one caller's own
  bookkeeping, where every live site passes the same expression twice.
  Folding would make it LOOSER and would drag `dbc-state` into owning a
  case-fold policy it does not have.
- `let _ = defect;` discarded the `Unparsable` reason. It is now carried
  into the rename-failure status, where „nelze zálohovat poškozený
  config.toml" otherwise said nothing about why it was thought poškozený.
- The §N pin is BRACE DEPTH rather than literal indentation. The
  positional version broke on a CRLF checkout and would break again on a
  `tab_spaces` change; depth 1 is the property actually meant. Verified
  non-vacuous — moving the call inside the `if` reports `Some(2)`.
- **§P cited „the codebase has already declined this twice in writing".
  There is exactly ONE prior decline** (Task 8's, on
  `editor_clobber_audit`, unchanged across the whole history of
  `main.rs`). The „second" was the sentence making the claim, added in the
  same pass that cited it. Recorded rather than quietly deleted, because a
  doc citing itself as corroboration is how a weak decision becomes
  load-bearing — and it is what this decline then rested on.
