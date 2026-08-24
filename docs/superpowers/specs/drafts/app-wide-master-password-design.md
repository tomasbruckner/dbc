# App-wide master password (design)

Date: 2026-08-24
Status: curated design from an explicit user requirement (verbatim intent
recorded below); decisions recorded here for review before execution.
Base: main @ v0.16.0.

Requirement (verbatim, 2026-08-24): „master heslo ma byt pro celou
aplikaci a ne per spojeni" — the master password should unlock once for
the whole app session, not prompt per connection.

## 0. Survey — the AS-IS flow, verified in source (basis for everything below)

### 0.1 Storage model is already app-wide

One vault file (`vault.bin`), ONE master password, secrets keyed by
connection id inside it (`crates/dbc-state/src/vault.rs:29-34`,
`BTreeMap<String, String>`). There is no per-connection password at rest —
Argon2id-derived key + ChaCha20Poly1305 envelope, key and plaintext
secrets zeroized on drop (`vault.rs:56-74`, hardened in the tech-debt
sweep).

### 0.2 Session cache is already app-wide

- Single window, single `AppView` (`main.rs:7892`, the only `open_window`
  call).
- `AppView.vault: Option<Vault>` (`main.rs:1006-1009`) — doc comment reads
  „Unlocked vault, kept for the session once the user has entered the
  master password once (brief: prompt on first use, not at startup)."
- Starts `None` at every launch (`main.rs:7950`); set exactly twice —
  successful unlock (`connections_ui.rs:2166`) and successful create
  (`connections_ui.rs:2214`) — and **never reset to `None` anywhere**
  (grep-verified: only those two `self.vault =` sites exist).

### 0.3 Prompt triggers — all lazy, all gated on the vault being locked

Exactly three sites open `ModalState::MasterPasswordPrompt` /
`CreateMasterPassword` (grep-verified; `connections_ui.rs:993-1002`),
each carrying a `PendingAfterUnlock` (`connections_ui.rs:971-985`):

| Trigger | Site | Gate | Pending |
|---|---|---|---|
| Connect (dropdown, and the palette's `Connection` row via `main.rs:3782-3792`) | `on_dropdown_item_click`, `connections_ui.rs:2019-2040` | `engine != Sqlite && self.vault.is_none() && Vault::exists(..)` (line 2026) | `Connect(id)` |
| „Test" in the connection dialog with an empty password field | `on_test_clicked` via `test_needs_vault_prompt`, `connections_ui.rs:1832-1849`, pure fn 2349-2356 | `password_empty && engine != Sqlite && !vault_unlocked && vault_file_exists` | `TestConnection(ui)` |
| „Uložit" with a non-empty password | `on_save_clicked`, `connections_ui.rs:1902-1923` | `!password.is_empty() && self.vault.is_none()`; file exists → unlock prompt, else → `CreateMasterPassword` | `SaveConnection(data)` |

After a successful unlock/create, `resume_pending`
(`connections_ui.rs:2120-2129`) resumes the interrupted action. Because
every gate checks `vault.is_none()` and the field is never cleared:

**Finding: in v0.16.0 there is NO code path that re-prompts after one
successful unlock. The prompt already fires at most once per app run.**

### 0.4 So what does the user actually experience as „per spojení"?

Three real, verified causes of the perception — none of them a
re-prompt-after-unlock bug:

1. **Every launch starts locked** (`main.rs:7950`), so the first connect
   of *every* run prompts — and the modal appears at the exact moment a
   specific connection row is clicked, so it reads as „this connection is
   asking for its password", not „the app is unlocking its vault".
2. **Cancel means re-prompt on the next need.** „Zrušit"
   (`cancel_master_password_prompt`, `connections_ui.rs:2150-2159`) closes
   the prompt without unlocking; the next connection click opens it again.
   A user who dismisses it (e.g. wanted to browse SQLite first) then
   clicks three connections sees three prompts — one per connection click.
   (Esc deliberately does NOT close it — `on_cancel_query`'s closable
   match, `_ => false` arm, main.rs:3494-3499/3520-3548.)
3. **The modal says nothing about scope.** Title is just „Master heslo"
   (`render_master_password_panel`, `connections_ui.rs:2627/2639`); there
   is no copy saying it unlocks the whole app once per session, and no
   app-level way to unlock or lock the vault deliberately — unlocking is
   only ever a side effect of a connection action.

Conclusion: the fix is small, as suspected — the session-wide unlock
already exists; the work is to make the unlock an **app-level act**
(palette actions + honest copy) and to pin the session invariant with
tests so it can never regress into actual per-connection prompting.

## 1. Decision: WHEN to prompt — lazy on first need, cached for the session

Options considered:

- (a) **Prompt at app start if any saved connection has a secret** —
  REJECTED. It taxes every SQLite-only or browse-only session (and the
  CLI-url startup path, `main.rs:7831/7987`) with a blocking modal for
  secrets the session may never touch. It also contradicts the existing,
  deliberate brief recorded on the field itself („prompt on first use, not
  at startup", main.rs:1007-1008).
- (b) **Lazy on first need, then cached unlocked for the session** —
  CHOSEN. This is the current behavior, kept verbatim; it already
  satisfies „jednou pro celou aplikaci" within a run. What changes is the
  framing and the app-level affordances (§2-§3), not the trigger model.

Explicitly deferred (recorded so it is a decision, not a TBD): removing
the once-per-LAUNCH prompt entirely would require persisting the derived
key in the OS credential store — exactly the opt-in `dbc-mcp setup`
already implements (`Vault::export_key` → Windows Credential Manager →
`Vault::unlock_with_key`, `dbc-mcp/src/main.rs:137-210`,
`vault.rs:145-194`). That is a real threat-model change for the GUI (any
process running as the user could then open every DB password without
knowing the master password), the user asked only for app-wide-not-
per-connection, and the mechanism already exists for whoever opts in via
the MCP path. Out of scope; a future opt-in („Odemykat automaticky přes
Credential Manager" in Settings) can reuse the two existing `Vault` APIs
unchanged.

## 2. Decision: session semantics + the perception fix

- `AppView.vault` stays the single session-wide holder — verified already
  true (§0.2); no structural change.
- **New invariant, pinned by tests (§6): once `self.vault.is_some()`, no
  code path may open `MasterPasswordPrompt`/`CreateMasterPassword`, and
  nothing except the explicit lock action (§3) may set `self.vault` back
  to `None`.** Today this holds by inspection; after this phase it holds
  by test.
- **Copy fix (cause §0.4/3):** the unlock modal stops pretending to be a
  connection gate. Concrete Czech strings:
  - `render_master_password_panel` title: „Master heslo" → **„Odemknout
    trezor"**, plus one explainer line under it: **„Master heslo platí pro
    celou aplikaci — zadáte ho nejvýše jednou za spuštění."**
  - `CreateMasterPassword` keeps the title „Vytvořit master heslo"
    (connections_ui.rs:2674) and gains the same explainer line.
  - Buttons („Odemknout" / „Vytvořit" / „Zrušit") unchanged — the Enter
    mapping via `ModalConfirmKind::UnlockVault/CreateVault`
    (connections_ui.rs:1204-1217) keeps working with zero changes because
    the kind derives from the `ModalState` variant, not from the pending.
- **App-level unlock (cause §0.4/1):** new palette action **„Odemknout
  trezor"** — shown only while `Vault::exists(&vault_path) &&
  vault.is_none()` — opens the same `MasterPasswordPrompt` with the new
  `PendingAfterUnlock::Nothing` (§4). The user who wants the prompt at a
  moment of their choosing (e.g. right after startup) gets it decoupled
  from any connection click; nobody else pays anything. No create-vault
  path here: when no vault file exists there is nothing to unlock (the
  action is hidden; the vault is created, as today, by the first save
  with a password).
- Esc behavior on the prompt stays as-is (not closable, `_ => false` arm)
  — one rule for both the lazy and the proactive prompt; „Zrušit" closes
  it. Diverging the two would re-litigate M6's „no accidental dismissal
  while a password is typed" rule for no user-visible gain.
- Cancel semantics unchanged (including the `TestConnection`
  dialog-restore path, connections_ui.rs:2131-2159). Cancelling still
  means „ask me again next time I need a secret" — that is correct: the
  app cannot connect without the secret, so the only alternatives are
  failing the connect with an auth error (worse) or nagging never (broken).

## 3. Decision: lock/relock — manual palette action only, no timer

- New palette action **„Zamknout trezor"** — shown only while
  `vault.is_some()`. Handler: `self.vault = None` (the `Drop` impl
  zeroizes key + secrets, vault.rs:56-74), status **„Trezor zamčen"**,
  `cx.notify()`. Pure state change; no modal, no confirmation (it is
  non-destructive — relocking loses nothing, the next need re-prompts).
- **No auto-lock timer.** YAGNI: the user asked for once-per-app, not for
  idle security; a timer adds a whole re-prompt lifecycle (mid-flight
  query? open apply dialog?) for a requirement nobody stated. Recorded as
  the explicit rejection so it is not re-proposed casually.
- Semantics after lock, stated honestly:
  - The ACTIVE connection keeps working — `get_secret` hands out owned
    `String`s (vault.rs:234) that were already baked into the dispatched
    connection spec; locking cannot claw those back (same lifetime
    reality as today's app-exit; documented, not new).
  - Any later secret-needing action re-prompts lazily (§1) — by
    construction, since every gate re-checks `vault.is_none()` live.
  - Edge already guarded: if a `SaveConnection` flow were somehow resumed
    against a locked vault, `finish_save`'s existing defensive arm
    (connections_ui.rs:1966-1973, „error: vault not unlocked") fails
    safely. The lock action cannot race the prompt anyway (palette and
    modal are mutually exclusive surfaces — `execute_palette_item` runs
    with the palette closed and the single-modal invariant holds), but
    the guard means even a future caller mistake degrades to a status
    line, not a panic or a silent plaintext loss.

## 4. Decision: `PendingAfterUnlock` — all three variants survive; one is added

With unlock kept lazy (§1), the three interruption sites remain the
normal path, so **nothing collapses**: `Connect(id)`,
`SaveConnection(Box<ConnectionFormData>)`, `TestConnection(Box<..Ui>)`
all stay exactly as they are (`connections_ui.rs:971-985`, resume at
2120-2129). What the enum gains:

- **`Nothing`** — the proactive palette unlock (§2) has no interrupted
  action to resume. `resume_pending` arm: set status **„Trezor
  odemčen"**, nothing else. (The create-submit path can also carry
  `Nothing` harmlessly if ever reached with it; today it cannot be —
  §2's visibility gate.) The redacted-`Debug` guard on the enum
  (connections_ui.rs:927-928) is unaffected — the new variant carries no
  data.

Rejected alternative: replacing the enum with a queued-closure design
(„run this after unlock"). The enum is exhaustively matched, redacts its
Debug, and has three call sites; closures would erase all three
properties to save four match arms.

## 5. Security analysis

- **Master password: still never stored** — not by the GUI (only the
  Argon2id-derived key + plaintext map live in `AppView.vault` for the
  session, zeroized on drop) and not by this design; §1 explicitly defers
  the only feature that would persist derived material.
- **Unlocked-vault lifetime:** unchanged upper bound (process lifetime,
  as today), with a NEW way to shorten it („Zamknout trezor" → immediate
  `wipe()` via drop). Strictly a tightening; no new exposure.
- **Threat-model deltas:** none negative. The retitled modal and the
  explainer string change zero crypto. The lock action's only caveat —
  secrets already copied into live connection specs are out of the
  vault's reach — is pre-existing (`get_secret` clones) and documented in
  §3 rather than papered over.
- **dbc-mcp path: unaffected — verified.** `dbc-mcp` opens `vault.bin`
  in its own process via `Vault::unlock_with_key` with the Credential-
  Manager-stored derived key (`dbc-mcp/src/main.rs:162`, `vault.rs:145-
  167`); it never talks to the GUI's `AppView.vault`. This design touches
  no `Vault` API, no envelope field, no KDF param — `unlock`,
  `unlock_with_key`, `export_key`, `persist` all byte-identical.
  GUI-side lock/unlock has no cross-process effect (each process holds
  its own `Vault`; concurrent `set_secret` vs MCP read already goes
  through `persist`'s atomic tmp+rename, unchanged).
- No new logging of field text; masked `TextField` rendering untouched;
  the M6 Esc rule is preserved, not weakened (§2).

## 6. Migration

**None — confirmed.** `vault.bin` envelope (kdf/m/t/p/salt/nonce/
ciphertext) unchanged; `config.toml` unchanged; no new persisted state of
any kind (palette actions derive visibility from live `AppView` state).
Old vaults open exactly as before (on-disk KDF params are already honored
per-file, vault.rs:107-113).

## 7. Tests (honest split)

Unit-testable (pure / entity-light, same tier as
`test_vault_prompt_tests`, connections_ui.rs:2359-2387):

- `fixed_actions` gating: „Odemknout trezor" present iff
  `vault_file_exists && !vault_unlocked`; „Zamknout trezor" present iff
  `vault_unlocked`; both absent when no vault file; **backup/restore rows
  remain the literal last two** (existing invariant test,
  palette.rs:199-203/581-582, must keep passing — new rows are inserted
  before the backup block, like `OpenChart`).
- `resume_pending` with `Nothing`: no modal, no connect dispatch, status
  set (structured like the existing cancel/restore tests around
  connections_ui.rs:3924).
- Lock action: `vault.is_some()` → after `lock_vault`, `vault.is_none()`
  and status „Trezor zamčen".
- **Invariant pin (§2):** with `vault_unlocked == true`,
  `test_needs_vault_prompt` is false for every engine (extends the
  existing `unlocked_vault_never_needs_prompt`, connections_ui.rs:2379),
  plus a table test asserting each of the three trigger gates' pure
  conditions are false once unlocked (extract the connect gate's
  condition into a small pure fn alongside `test_needs_vault_prompt` so
  it is testable the same way).
- `test_needs_vault_prompt` existing tests: untouched, must keep passing.

Visual pass (no GPUI harness, as established in the UX-polish design §7):
palette shows/hides the two actions as the vault state flips; proactive
unlock → status „Trezor odemčen", then clicking two different secret-
needing connections prompts ZERO times; lock → next connection click
prompts once; retitled modal + explainer renders in both themes.

## 8. Task decomposition + risks

A SMALL phase — two tasks, serialized through `connections_ui.rs`/
`main.rs` (same single-worktree posture as the UX-polish phase):

- **T1 — connections_ui.rs:** `PendingAfterUnlock::Nothing` + resume arm
  („Trezor odemčen"); `lock_vault(&mut self, cx)` + „Trezor zamčen";
  `open_unlock_vault_prompt(&mut self, window, cx)` (single-modal guard
  like every opener, `MasterPasswordPrompt` with `pending: Nothing`);
  modal retitle „Odemknout trezor" + explainer line in
  `render_master_password_panel` (and explainer in the create panel);
  connect-gate condition extracted next to `test_needs_vault_prompt`;
  unit tests per §7.
- **T2 — palette.rs + main.rs (after T1, small):**
  `PaletteAction::{UnlockVault, LockVault}`; `fixed_actions` gains
  `vault_unlockable: bool, vault_lockable: bool` (rows inserted before
  the backup block — last-two-rows invariant); caller passes
  `Vault::exists(&self.vault_path) && self.vault.is_none()` /
  `self.vault.is_some()`; `execute_palette_item` arms →
  `open_unlock_vault_prompt` / `lock_vault`; `fixed_actions` tests.

Version bump + merge checklist as the closing step of T2.

Risks / needs-verification:

- **The requirement may partly target the per-LAUNCH prompt** (§0.4/1).
  This phase makes the scope explicit in the UI and gives a proactive
  unlock, but a prompt on first use after every start remains — by
  design (§1's deferral). If the user's follow-up is „a mezi spuštěními?",
  the answer is the recorded Credential-Manager opt-in follow-up, not a
  rework of this phase.
- `fixed_actions` signature grows to six params — cosmetic; if a seventh
  gate ever appears, fold them into a small `PaletteGates` struct then
  (noted, not done now — five call-site churn for zero behavior).
- „Zamknout trezor" while a background schema fetch is resolving: the
  fetch already holds its own spec copy (§3 lifetime note) — expected
  no-op interaction; confirm once in the visual pass.
