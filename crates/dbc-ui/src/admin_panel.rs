//! G10 T4/T5: "Správa serveru" — the admin panel shell (T4), "Role a
//! členství" (T4), and "Oprávnění" (T5, the engine-aware
//! GRANT/REVOKE/DENY privileges matrix). T6 extends this same file with
//! "Databáze a schémata".
//!
//! Layout of this file:
//!   1. `AdminEntry`/`admin_entry_state` — the entry-point gate (design §2):
//!      pure and unit-tested, the UI-level half of CURATION item 6 (the
//!      runner's `guard_not_read_only` is the OTHER half — the shared
//!      choke point every admin write still goes through, unchanged).
//!   2. `RoleRow`/`parse_roles`, `Membership`/`parse_memberships` — pure
//!      catalog-row parsers, engine-agnostic (works for pg_roles and both
//!      MSSQL principal queries without per-engine structs).
//!   3. `MembershipEdits` — staged membership diff, mirrors
//!      `sandbox::EditState`'s staging idiom (toggle stages, toggle again
//!      un-stages, `to_statements` builds the final `WriteStatement`s).
//!   4. `MatrixState` (T5) — one (schema, grantee) scope's privileges
//!      matrix: committed state parsed from `admin_sql::privileges_catalog`'s
//!      ACL rows, staged state as a diff map, same "click cycles via
//!      `admin_sql::cycle_cell`, staging back to committed clears the
//!      entry" idiom `MembershipEdits` uses.
//!   5. `AdminModal`/`AdminPanel`/`AdminEvent` — the GPUI entity: owns the
//!      parsed catalog + staged edits, renders via plain divs (a sub-nav
//!      strip, NOT the result-tab strip — design §2), and emits
//!      `AdminEvent`s for the things it can't do itself (`main.rs` owns the
//!      `QueryRunner` and the confirm dialog).
//!
//! §3-novela (Global Constraints): every admin write staged here reaches
//! `Connection::execute` ONLY through `main.rs`'s one confirm modal (which
//! shows `display_sql`, '***'-redacted) → `QueryRunner::run_write_transaction`
//! → the shared `guard_not_read_only` choke point. This file never calls
//! `execute` itself, never renders `exec_sql`, and never logs/asserts a
//! `WriteStatement`'s `Debug` output (whose manual impl already redacts
//! `exec_sql` — see `admin_sql::WriteStatement`).

use std::collections::{BTreeSet, HashMap};

use dbc_core::SchemaSnapshot;
use dbc_state::Engine;
use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, ClickEvent, Context, Entity,
    EventEmitter, FocusHandle, Focusable, Window,
};

use crate::admin_sql::{self, CellState, WriteStatement};
use crate::theme::{ActiveTheme, Theme};
use crate::connections_ui;
use crate::runner::AdminCatalogRows;

/// Tab-strip singleton dedup key (`tabs.rs::ResultTab::preview_key`) —
/// there is only ever one admin tab per connection at a time (design §2:
/// "one tab, per connection, singleton").
pub const ADMIN_PREVIEW_KEY: &str = "__admin__";

// ---------------------------------------------------------------------
// 1. Entry-point gate (design §2, CURATION item 6's UI-level half).
// ---------------------------------------------------------------------

/// The entry-point gate (design §2), pure and unit-tested — the UI-level
/// half of CURATION item 6 (the runner's `guard_not_read_only` is the
/// OTHER half, unchanged, still the sole write choke point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminEntry {
    /// SQLite (feature-exempt, design §0), or no active connection at all.
    /// MSSQL was Hidden here through G15 T3-T7 (the admin write path had
    /// never run live) — T8's live validation
    /// (`mssql_admin_catalogs_round_trip`/
    /// `mssql_admin_builder_mutation_round_trip`) flipped it to the normal
    /// read_only-gated Disabled/Enabled logic, same ON-flip discipline as
    /// `dialect_for_engine`/`detect_editable_pk`. In every Hidden case the
    /// tree row and palette action are both absent entirely.
    Hidden,
    /// A real (currently: pg-only) read-only connection — the tree row
    /// renders greyed with a "pouze pro čtení" hint; the palette has no
    /// disabled-row idiom, so its entry is simply omitted too (same as
    /// `Hidden` there — see `fixed_actions`).
    Disabled,
    Enabled,
}

/// `engine`/`read_only` — the same three-way lookup `AppView::active_engine`/
/// `active_read_only` already resolve (saved config → `cfg.engine`/
/// `cfg.read_only`; CLI-arg URL → `engine_from_url`, never read-only;
/// neither → `None`).
pub fn admin_entry_state(engine: Option<Engine>, read_only: bool) -> AdminEntry {
    match engine {
        None => AdminEntry::Hidden,
        Some(Engine::Sqlite) => AdminEntry::Hidden,
        // G16: embedded engine — no roles/privileges/logins to administer;
        // same posture as Sqlite.
        Some(Engine::Duckdb) => AdminEntry::Hidden,
        // G15 T8 ON-flip: the admin GRANT/REVOKE/DENY write path against
        // MSSQL is now live-validated —
        // `mssql_admin_catalogs_round_trip`/`mssql_admin_builder_mutation_round_trip`
        // (runner.rs's `mssql_docker_tests`) ran `roles_catalog`/
        // `privileges_catalog`/`sizes_catalog` (incl. the flagged
        // `schema_sizes` empty-schema LEFT JOIN shape) and a full
        // CREATE LOGIN/USER -> GRANT/DENY/REVOKE -> membership -> DROP
        // round-trip live, so MSSQL now falls through to the same
        // read_only-gated Disabled/Enabled logic every other writable
        // engine already uses — no longer Hidden.
        Some(_) if read_only => AdminEntry::Disabled,
        Some(_) => AdminEntry::Enabled,
    }
}

/// The three sub-views inside the admin panel (design §2's sub-nav).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminSubView {
    Roles,
    /// T5.
    Privileges,
    /// T6.
    Databases,
}

// ---------------------------------------------------------------------
// 2. Pure catalog-row parsers.
// ---------------------------------------------------------------------

/// Generic parsed role row: first result column is the name, remaining
/// columns become (header, value) detail pairs (NULL → "—") — works for
/// `pg_roles` and both MSSQL principal queries without per-engine structs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRow {
    pub name: String,
    pub detail: Vec<(String, String)>,
}

pub fn parse_roles(rows: &AdminCatalogRows) -> Vec<RoleRow> {
    let (cols, data) = rows;
    data.iter()
        .map(|row| {
            let name = row.first().cloned().flatten().unwrap_or_default();
            let detail = cols
                .iter()
                .skip(1)
                .zip(row.iter().skip(1))
                .map(|(h, v)| (h.clone(), v.clone().unwrap_or_else(|| "—".to_string())))
                .collect();
            RoleRow { name, detail }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub role: String,
    pub member: String,
    pub server_role: bool,
}

/// Columns `role`, `member`[, `admin_option`] (extras ignored) — by NAME,
/// not position, so pg's `membership`/MSSQL's `db_role_members`/
/// `server_role_members` (all shaped `(role, member[, ...])`, see
/// `admin_sql::roles_catalog`) share this one parser.
pub fn parse_memberships(rows: &AdminCatalogRows, server_role: bool) -> Vec<Membership> {
    let (cols, data) = rows;
    let role_ix = cols.iter().position(|c| c == "role").unwrap_or(0);
    let member_ix = cols.iter().position(|c| c == "member").unwrap_or(1);
    data.iter()
        .map(|row| Membership {
            role: row.get(role_ix).cloned().flatten().unwrap_or_default(),
            member: row.get(member_ix).cloned().flatten().unwrap_or_default(),
            server_role,
        })
        .collect()
}

// ---------------------------------------------------------------------
// 3. Staged membership diff (mirrors sandbox::EditState's staging idiom).
// ---------------------------------------------------------------------

/// Staged membership diff. `toggle` stages the OPPOSITE of the row's
/// COMMITTED state (`currently_member`, the original catalog fact — not
/// the effective/staged one) and toggling the SAME cell again un-stages it
/// (removes the diff entry) rather than accumulating a redundant flip —
/// same "staging back to committed clears the tint" idiom
/// `sandbox::EditState` uses for cell edits.
#[derive(Default)]
pub struct MembershipEdits {
    pub add: BTreeSet<(String, String, bool)>, // (role, member, server_role)
    pub remove: BTreeSet<(String, String, bool)>,
}

impl MembershipEdits {
    pub fn toggle(&mut self, role: &str, member: &str, server_role: bool, currently_member: bool) {
        let key = (role.to_string(), member.to_string(), server_role);
        if currently_member {
            if !self.remove.remove(&key) {
                self.remove.insert(key);
            }
        } else if !self.add.remove(&key) {
            self.add.insert(key);
        }
    }

    /// Effective checkbox state after staging: an explicit remove/add
    /// staging always wins over the committed fact; otherwise the
    /// committed fact stands.
    pub fn is_checked(&self, role: &str, member: &str, server_role: bool, currently_member: bool) -> bool {
        let key = (role.to_string(), member.to_string(), server_role);
        if self.remove.contains(&key) {
            false
        } else if self.add.contains(&key) {
            true
        } else {
            currently_member
        }
    }

    pub fn change_count(&self) -> usize {
        self.add.len() + self.remove.len()
    }

    /// Kept for API parity with `sandbox::EditState`'s staging idiom (and
    /// with this struct's own `change_count`/`clear`/`to_statements`) even
    /// though `AdminPanel::is_dirty` composes its OWN answer from
    /// `AdminPanel::change_count` (which folds `staged_role_actions` in
    /// too) rather than calling this — that keeps the panel's dirtiness a
    /// single definition (`combined_change_count`) instead of two.
    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.change_count() > 0
    }

    pub fn clear(&mut self) {
        self.add.clear();
        self.remove.clear();
    }

    /// Adds first (in `(role, member, server_role)` sorted order), then
    /// removes — deterministic statement order for the confirm dialog.
    /// `admin_option`/`WITH ADMIN OPTION` is never staged via the
    /// membership checkbox (a plain GRANT/`ADD MEMBER`) — always `false`.
    pub fn to_statements(&self, engine: Engine) -> Vec<WriteStatement> {
        let mut out = Vec::new();
        for (role, member, server_role) in &self.add {
            out.extend(admin_sql::add_membership(engine, role, member, false, *server_role));
        }
        for (role, member, server_role) in &self.remove {
            out.extend(admin_sql::remove_membership(engine, role, member, *server_role));
        }
        out
    }
}

/// Pure, GPUI-free half of `AdminPanel::change_count`'s arithmetic —
/// `staged_role_actions` (create-role/change-password/drop-role, staged
/// directly with no local diffing of their own) plus `membership_edits`'s
/// own count plus (T5) `matrix_changes` (the Privileges sub-view's staged
/// cell count — `MatrixState::change_count`). Extracted so the tab-strip
/// "✕" close guard's dirtiness contract (`main.rs::AppView::
/// grid_dirty_change_count`'s `Admin` arm — review finding: that match had
/// NO `Admin` arm at all, so closing a dirty admin tab silently discarded
/// staged writes) is directly unit-testable without constructing a GPUI
/// `AdminPanel` entity. This is the SINGLE dirtiness definition for the
/// whole panel — T5/T6 sub-views fold their own staged-edit counts into
/// this same function rather than inventing a second one (in practice at
/// most one of the three terms is ever non-zero at once, since switching
/// sub-view clears the others — see `AdminPanel::switch_sub_view` — but the
/// formula stays valid regardless).
fn combined_change_count(
    staged_role_actions: usize,
    membership_edits: &MembershipEdits,
    matrix_changes: usize,
) -> usize {
    staged_role_actions + membership_edits.change_count() + matrix_changes
}

// ---------------------------------------------------------------------
// 4. Privileges matrix (T5) — engine-aware GRANT/REVOKE/DENY.
// ---------------------------------------------------------------------

/// MSSQL `sys.database_permissions.state_desc` → `CellState` (design §1):
/// `DENY` → Denied, `GRANT`/`GRANT_WITH_GRANT_OPTION` → Granted — anything
/// else (a `REVOKE` state never actually appears as a row; defensive only)
/// → NotSet.
fn mssql_state_from_desc(state_desc: &str) -> CellState {
    match state_desc {
        "DENY" => CellState::Denied,
        "GRANT" | "GRANT_WITH_GRANT_OPTION" => CellState::Granted,
        _ => CellState::NotSet,
    }
}

/// The privileges matrix's cell glyph (design §2, Grounding): `✓` Granted,
/// `✗` Denied (MSSQL only — pg's bi-state `cycle_cell` never produces it),
/// empty NotSet. Shared by the object grid and the fixed schema/db rows.
fn privilege_glyph(state: CellState) -> &'static str {
    match state {
        CellState::Granted => "✓",
        CellState::Denied => "✗",
        CellState::NotSet => "",
    }
}

/// One (schema, grantee) scope's matrix — committed state parsed from
/// `admin_sql::privileges_catalog`'s ACL rows, staged state as a diff map
/// (only cells that differ from committed are present at all — the same
/// "staging back to committed removes the entry" idiom `MembershipEdits`
/// uses).
#[derive(Default)]
pub struct MatrixState {
    pub objects: Vec<String>,
    pub current: HashMap<(String, String), CellState>, // (object, privilege)
    pub staged: HashMap<(String, String), CellState>,
    pub schema_current: HashMap<String, CellState>, // privilege -> state
    pub schema_staged: HashMap<String, CellState>,
    pub db_current: HashMap<String, CellState>, // pg only
    pub db_staged: HashMap<String, CellState>,
}

impl MatrixState {
    /// Postgres rows (`object_acl`/`schema_acl`/`db_acl`): grantee-filtered,
    /// `privilege_type` → Granted. MSSQL rows (`object_perms`/
    /// `schema_perms`): `state_desc` mapped via `mssql_state_from_desc`.
    /// `objects` = every distinct object seen in the object-level rows,
    /// REGARDLESS of grantee (`aclexplode` over `acldefault` yields an
    /// owner-default row for EVERY object, so an object the selected
    /// grantee has zero privileges on still appears as a matrix row, all
    /// cells NotSet). Columns are looked up BY NAME (not fixed position),
    /// so the exact column set `admin_sql`'s queries declare is what's
    /// actually read.
    ///
    /// `engine` is unused in the body — which engine produced `labeled` is
    /// already implicit in the LABELS present (`object_acl` vs.
    /// `object_perms`, etc.) and in whether a `state_desc` column exists at
    /// all (pg has none; presence alone routes to `mssql_state_from_desc`).
    /// Kept as a parameter anyway for signature parity with `click_cell`/
    /// `click_schema_cell`/`to_statements` (every other `MatrixState`
    /// method that touches privilege semantics takes one) and because the
    /// plan's own interface spec declares it.
    pub fn from_catalog(
        _engine: Engine,
        grantee: &str,
        labeled: &[(&'static str, AdminCatalogRows)],
    ) -> MatrixState {
        let mut m = MatrixState::default();
        let mut objects: BTreeSet<String> = BTreeSet::new();

        for (label, data) in labeled {
            let (cols, rows) = data;
            let ix = |name: &str| cols.iter().position(|c| c == name);
            match *label {
                "object_acl" | "object_perms" => {
                    let Some(object_ix) = ix("object").or_else(|| ix("object_name")) else { continue };
                    let Some(grantee_ix) = ix("grantee") else { continue };
                    let priv_col = if *label == "object_acl" { "privilege_type" } else { "permission_name" };
                    let Some(priv_ix) = ix(priv_col) else { continue };
                    let state_ix = ix("state_desc"); // MSSQL only
                    for row in rows {
                        let Some(object) = row.get(object_ix).cloned().flatten() else { continue };
                        objects.insert(object.clone());
                        let row_grantee = row.get(grantee_ix).cloned().flatten().unwrap_or_default();
                        if row_grantee != grantee {
                            continue;
                        }
                        let Some(priv_name) = row.get(priv_ix).cloned().flatten() else { continue };
                        let state = match state_ix {
                            Some(six) => row
                                .get(six)
                                .cloned()
                                .flatten()
                                .map(|s| mssql_state_from_desc(&s))
                                .unwrap_or(CellState::Granted),
                            None => CellState::Granted, // pg: presence in the ACL IS Granted.
                        };
                        m.current.insert((object, priv_name), state);
                    }
                }
                "schema_acl" | "schema_perms" => {
                    let Some(grantee_ix) = ix("grantee") else { continue };
                    let priv_col = if *label == "schema_acl" { "privilege_type" } else { "permission_name" };
                    let Some(priv_ix) = ix(priv_col) else { continue };
                    let state_ix = ix("state_desc");
                    for row in rows {
                        let row_grantee = row.get(grantee_ix).cloned().flatten().unwrap_or_default();
                        if row_grantee != grantee {
                            continue;
                        }
                        let Some(priv_name) = row.get(priv_ix).cloned().flatten() else { continue };
                        let state = match state_ix {
                            Some(six) => row
                                .get(six)
                                .cloned()
                                .flatten()
                                .map(|s| mssql_state_from_desc(&s))
                                .unwrap_or(CellState::Granted),
                            None => CellState::Granted,
                        };
                        m.schema_current.insert(priv_name, state);
                    }
                }
                "db_acl" => {
                    // pg only (design §2 — MSSQL has no db-level row).
                    let (Some(grantee_ix), Some(priv_ix)) = (ix("grantee"), ix("privilege_type")) else {
                        continue;
                    };
                    for row in rows {
                        let row_grantee = row.get(grantee_ix).cloned().flatten().unwrap_or_default();
                        if row_grantee != grantee {
                            continue;
                        }
                        let Some(priv_name) = row.get(priv_ix).cloned().flatten() else { continue };
                        m.db_current.insert(priv_name, CellState::Granted);
                    }
                }
                _ => {}
            }
        }

        m.objects = objects.into_iter().collect();
        m
    }

    /// Staged wins over committed; NotSet is the default for an
    /// unmentioned object/privilege pair.
    pub fn effective(&self, object: &str, privilege: &str) -> CellState {
        let key = (object.to_string(), privilege.to_string());
        self.staged.get(&key).copied().unwrap_or_else(|| self.current.get(&key).copied().unwrap_or(CellState::NotSet))
    }

    /// Cycle one cell via `admin_sql::cycle_cell`; staging back to the
    /// committed state REMOVES the diff entry (yellow tint clears) instead
    /// of leaving a redundant "staged = committed" entry around.
    pub fn click_cell(&mut self, engine: Engine, object: &str, privilege: &str) {
        let key = (object.to_string(), privilege.to_string());
        let next = admin_sql::cycle_cell(engine, self.effective(object, privilege));
        let committed = self.current.get(&key).copied().unwrap_or(CellState::NotSet);
        if next == committed {
            self.staged.remove(&key);
        } else {
            self.staged.insert(key, next);
        }
    }

    pub fn click_schema_cell(&mut self, engine: Engine, privilege: &str) {
        let key = privilege.to_string();
        let effective = self.schema_staged.get(&key).copied().unwrap_or_else(|| {
            self.schema_current.get(&key).copied().unwrap_or(CellState::NotSet)
        });
        let next = admin_sql::cycle_cell(engine, effective);
        let committed = self.schema_current.get(&key).copied().unwrap_or(CellState::NotSet);
        if next == committed {
            self.schema_staged.remove(&key);
        } else {
            self.schema_staged.insert(key, next);
        }
    }

    /// pg only — bi-state by construction (always cycles via
    /// `Engine::Postgres`, since `admin_sql::database_privilege_pg` is
    /// pg-only too and the UI never renders this row for MSSQL).
    pub fn click_db_cell(&mut self, privilege: &str) {
        let key = privilege.to_string();
        let effective =
            self.db_staged.get(&key).copied().unwrap_or_else(|| self.db_current.get(&key).copied().unwrap_or(CellState::NotSet));
        let next = admin_sql::cycle_cell(Engine::Postgres, effective);
        let committed = self.db_current.get(&key).copied().unwrap_or(CellState::NotSet);
        if next == committed {
            self.db_staged.remove(&key);
        } else {
            self.db_staged.insert(key, next);
        }
    }

    pub fn change_count(&self) -> usize {
        self.staged.len() + self.schema_staged.len() + self.db_staged.len()
    }

    /// Kept for API/interface parity (the plan's own spec, and
    /// `MembershipEdits::is_dirty`'s identical situation) even though
    /// `AdminPanel::is_dirty` composes its answer from `AdminPanel::
    /// change_count`/`combined_change_count` instead of calling this — one
    /// dirtiness definition for the whole panel, not one per sub-view.
    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.change_count() > 0
    }

    pub fn clear(&mut self) {
        self.staged.clear();
        self.schema_staged.clear();
        self.db_staged.clear();
    }

    /// Groups staged OBJECT cells by `(object, target state)` so multi-priv
    /// changes emit one `"GRANT SELECT, INSERT ON …"` statement (design
    /// §3's table) rather than one per cell — iterated in deterministic
    /// `(object, privilege-column)` order: `self.objects` is already sorted
    /// (built from a `BTreeSet` in `from_catalog`), and within an object,
    /// privilege columns follow `PG_TABLE_PRIVS`/`MSSQL_TABLE_PRIVS`'s
    /// declared order, not staging/click order. Schema/db cells are one
    /// statement each, in `SCHEMA_PRIVS`/`PG_DATABASE_PRIVS` order. `Err`
    /// bubbles `admin_sql`'s own refusals (unreachable via the UI cycles —
    /// pg never stages Denied, SQLite never renders this sub-view at all —
    /// the errors-are-values backstop, not a real UI path).
    pub fn to_statements(
        &self,
        engine: Engine,
        schema: &str,
        grantee: &str,
        database: &str,
    ) -> Result<Vec<WriteStatement>, String> {
        let priv_columns: &[&str] = match engine {
            Engine::Postgres => admin_sql::PG_TABLE_PRIVS,
            Engine::Mssql => admin_sql::MSSQL_TABLE_PRIVS,
            // G16: Duckdb joins Sqlite — admin is Hidden for both, no
            // privilege columns exist (admin_entry_state).
            Engine::Sqlite | Engine::Duckdb => &[],
        };
        let mut out = Vec::new();

        for object in &self.objects {
            let mut granted: Vec<&str> = Vec::new();
            let mut revoked: Vec<&str> = Vec::new();
            let mut denied: Vec<&str> = Vec::new();
            for &priv_name in priv_columns {
                let key = (object.clone(), priv_name.to_string());
                if let Some(&target) = self.staged.get(&key) {
                    match target {
                        CellState::Granted => granted.push(priv_name),
                        CellState::NotSet => revoked.push(priv_name),
                        CellState::Denied => denied.push(priv_name),
                    }
                }
            }
            if !granted.is_empty() {
                out.push(admin_sql::object_privilege(engine, schema, object, &granted, grantee, CellState::Granted)?);
            }
            if !revoked.is_empty() {
                out.push(admin_sql::object_privilege(engine, schema, object, &revoked, grantee, CellState::NotSet)?);
            }
            if !denied.is_empty() {
                out.push(admin_sql::object_privilege(engine, schema, object, &denied, grantee, CellState::Denied)?);
            }
        }

        for &priv_name in admin_sql::SCHEMA_PRIVS {
            if let Some(&target) = self.schema_staged.get(priv_name) {
                out.push(admin_sql::schema_privilege(engine, schema, priv_name, grantee, target)?);
            }
        }

        for &priv_name in admin_sql::PG_DATABASE_PRIVS {
            if let Some(&target) = self.db_staged.get(priv_name) {
                out.push(admin_sql::database_privilege_pg(database, priv_name, grantee, target)?);
            }
        }

        Ok(out)
    }
}

/// Distinct, sorted schema names from a `SchemaSnapshot`'s tables — feeds
/// the Privileges sub-view's schema selector (design §2). SQLite's
/// single-implicit-schema snapshots (every `TableInfo.schema` is `None`)
/// yield an empty list — harmless, since the admin panel is `Hidden` for
/// SQLite entirely (`admin_entry_state`), so this is never actually
/// reached for that engine.
pub fn distinct_schemas(snapshot: &SchemaSnapshot) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for t in &snapshot.tables {
        if let Some(s) = &t.schema {
            set.insert(s.clone());
        }
    }
    set.into_iter().collect()
}

/// `AdminPanel::apply_catalog`'s routing predicate: `true` when `rows`
/// carries any of `admin_sql::privileges_catalog`'s labels — pulled out as
/// a standalone, pure function so the label-detection logic (which decides
/// whether a batch is routed whole into `MatrixState::from_catalog`, or
/// per-label into the Roles parsers) is directly unit-testable without
/// constructing an `AdminPanel`/GPUI entity.
fn is_privileges_batch(rows: &[(&'static str, AdminCatalogRows)]) -> bool {
    rows.iter().any(|(l, _)| matches!(*l, "object_acl" | "schema_acl" | "db_acl" | "object_perms" | "schema_perms"))
}

// ---------------------------------------------------------------------
// 5. Databases & schemas sizes (T6) — read-only lists + direct-to-confirm
//    schema DDL.
// ---------------------------------------------------------------------

/// `AdminPanel::apply_catalog`'s routing predicate for T6, same shape as
/// `is_privileges_batch` — `true` when `rows` carries any of
/// `admin_sql::sizes_catalog`'s labels (identical label set on both
/// engines: `current_db_size`/`databases`/`schema_sizes`).
fn is_sizes_batch(rows: &[(&'static str, AdminCatalogRows)]) -> bool {
    rows.iter().any(|(l, _)| matches!(*l, "current_db_size" | "databases" | "schema_sizes"))
}

/// "1.2 GB" / "340.5 MB" / "512 B" — binary units, one decimal above B (no
/// decimal at all under 1 KB, matching `0 B`/`512 B`'s exact test shape).
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

/// Bar width fraction in `[0, 1]`; `0` when `max` is `0` (an all-empty list
/// — nothing to compare against, not a divide-by-zero crash). Clamped
/// defensively in case a size query races a concurrent DDL and `bytes`
/// ends up briefly larger than the `max` computed a moment earlier.
pub fn bar_fraction(bytes: u64, max: u64) -> f32 {
    if max == 0 {
        return 0.0;
    }
    (bytes as f64 / max as f64).clamp(0.0, 1.0) as f32
}

/// pg `"databases"` rows (`datname`, `bytes`) → `(name, Some(bytes))`.
/// MSSQL `"databases"` rows (`name`, `database_id`, `create_date`,
/// `state_desc`) carry NO per-db size at all (`sys.databases` has none) →
/// `(name, None)` — the render shows "—", no bar.
pub fn parse_db_sizes(engine: Engine, rows: &AdminCatalogRows) -> Vec<(String, Option<u64>)> {
    let (cols, data) = rows;
    match engine {
        Engine::Postgres => {
            let name_ix = cols.iter().position(|c| c == "datname").unwrap_or(0);
            let bytes_ix = cols.iter().position(|c| c == "bytes");
            data.iter()
                .map(|row| {
                    let name = row.get(name_ix).cloned().flatten().unwrap_or_default();
                    let bytes = bytes_ix
                        .and_then(|ix| row.get(ix))
                        .cloned()
                        .flatten()
                        .and_then(|s| s.parse::<u64>().ok());
                    (name, bytes)
                })
                .collect()
        }
        // G16: Duckdb defensive — never called (admin Hidden), same
        // no-per-db-size posture as MSSQL/Sqlite.
        Engine::Mssql | Engine::Sqlite | Engine::Duckdb => {
            let name_ix = cols.iter().position(|c| c == "name").unwrap_or(0);
            data.iter()
                .map(|row| (row.get(name_ix).cloned().flatten().unwrap_or_default(), None))
                .collect()
        }
    }
}

/// pg `"schema_sizes"` rows (`schema`, `bytes`) — a NULL `SUM` (a schema
/// with no tables at all) reads as `0`, not a parse error/crash. MSSQL
/// (`schema_name`, `reserved_kb`, `used_kb`) — `reserved_kb` converted to
/// bytes (`* 1024`).
pub fn parse_schema_sizes(engine: Engine, rows: &AdminCatalogRows) -> Vec<(String, u64)> {
    let (cols, data) = rows;
    match engine {
        Engine::Postgres => {
            let schema_ix = cols.iter().position(|c| c == "schema").unwrap_or(0);
            let bytes_ix = cols.iter().position(|c| c == "bytes");
            data.iter()
                .map(|row| {
                    let schema = row.get(schema_ix).cloned().flatten().unwrap_or_default();
                    let bytes = bytes_ix
                        .and_then(|ix| row.get(ix))
                        .cloned()
                        .flatten()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    (schema, bytes)
                })
                .collect()
        }
        Engine::Mssql | Engine::Sqlite | Engine::Duckdb => {
            let schema_ix = cols.iter().position(|c| c == "schema_name").unwrap_or(0);
            let kb_ix = cols.iter().position(|c| c == "reserved_kb");
            data.iter()
                .map(|row| {
                    let schema = row.get(schema_ix).cloned().flatten().unwrap_or_default();
                    let kb = kb_ix
                        .and_then(|ix| row.get(ix))
                        .cloned()
                        .flatten()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    (schema, kb * 1024)
                })
                .collect()
        }
    }
}

/// The Grounding's headline line's value half (`"Aktuální databáze: {…}"`
/// — the prefix is added by the render, not here). Postgres's
/// `current_db_size` query already ships a human-formatted `pretty` column
/// (`pg_size_pretty`) — used verbatim, not re-derived through
/// `format_bytes` (so it stays byte-for-byte what a DBA would expect from
/// `pg_size_pretty`). MSSQL has no such column (`sys.database_files`'s
/// `data_mb`/`log_mb` are raw numbers) — summed and run through
/// `format_bytes` instead. `None` when the row/columns aren't there yet
/// (loading) or can't be parsed.
pub fn current_db_size_label(engine: Engine, rows: &AdminCatalogRows) -> Option<String> {
    let (cols, data) = rows;
    let row = data.first()?;
    match engine {
        Engine::Postgres => {
            let ix = cols.iter().position(|c| c == "pretty")?;
            row.get(ix).cloned().flatten()
        }
        Engine::Mssql => {
            let data_ix = cols.iter().position(|c| c == "data_mb")?;
            let log_ix = cols.iter().position(|c| c == "log_mb")?;
            let data_mb: f64 = row.get(data_ix).cloned().flatten()?.parse().ok()?;
            let log_mb: f64 = row.get(log_ix).cloned().flatten()?.parse().ok()?;
            let bytes = ((data_mb + log_mb) * 1024.0 * 1024.0).round() as u64;
            Some(format_bytes(bytes))
        }
        Engine::Sqlite | Engine::Duckdb => None,
    }
}

// ---------------------------------------------------------------------
// 6. GPUI entity.
// ---------------------------------------------------------------------

/// T6: a fixed-width "track" with a proportionally-filled "bar" — shared by
/// `render_databases_body`'s two size lists (databases; schemas). Plain
/// `div`s, not a dedicated bar-chart primitive; `fraction` is already
/// clamped to `[0, 1]` by `bar_fraction`.
fn render_size_bar(fraction: f32, theme: &Theme) -> impl IntoElement {
    div().w(px(160.)).h(px(10.)).bg(theme.bg_hover).rounded_sm().child(
        div().w(px(160. * fraction)).h(px(10.)).bg(theme.accent).rounded_sm(),
    )
}

/// Which boolean flag on `AdminModal::NewRole` a checkbox click toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleFlagKind {
    Login,
    Superuser,
    Createdb,
    Createrole,
}

/// T5: the action `AdminPanel::discard_confirm_yes` performs on "Zahodit",
/// or drops entirely on "Zpět" (`discard_confirm_no`) — generalizes T4's
/// "sub-view switch only" shape (`SwitchSubView`) to the two NEW actions a
/// dirty Privileges sub-view can defer: re-selecting the schema or the
/// grantee (`request_select_schema`/`request_select_grantee`). Not `Copy`
/// (the `String` payloads), so `render_discard_confirm_overlay` only checks
/// `self.discard_confirm.is_some()`/`.as_ref()` — it doesn't need to know
/// WHICH action is pending to render the generic "Zahodit neuložené
/// změny?" prompt.
#[derive(Clone)]
enum PendingAdminAction {
    SwitchSubView(AdminSubView),
    SelectSchema(String),
    SelectGrantee(String),
}

/// UX-polish sweep #9: whether Esc may close an admin modal — the M6
/// password rule (never dismiss a modal holding a typed password) as a
/// pure decision, same tier as connections_ui's `blocks_clipboard_write`.
fn admin_esc_closable(has_password_field: bool, password_empty: bool) -> bool {
    !has_password_field || password_empty
}

/// Panel-local overlay state — same visual idiom as the grid cell editor
/// (a floating panel over the tab content, not a full-window modal). While
/// the modal is open, the password sits in the `TextField`'s own buffer —
/// a plain, unprotected `String` buffer, same as every other password
/// `TextField` in this app (e.g. the vault master-password prompt); this
/// struct does not harden that pre-existing convention. What CURATION item
/// 4 actually guarantees is downstream of that: the confirm handler reads
/// the buffer ONCE into a `zeroize::Zeroizing<String>` and that's the only
/// form the password takes anywhere else — never cached a second time in
/// this struct, in `AdminPanel`, or beyond the resulting `WriteStatement`'s
/// `exec_sql`.
enum AdminModal {
    NewRole {
        name: Entity<connections_ui::TextField>,
        password: Entity<connections_ui::TextField>,
        login: bool,
        superuser: bool,
        createdb: bool,
        createrole: bool,
    },
    ChangePassword {
        role: String,
        password: Entity<connections_ui::TextField>,
    },
    /// T6: direct-to-confirm (design §2/"Resolved design ambiguities" item
    /// 6 — unlike Roles' `staged_role_actions`, schema mutations are never
    /// locally staged; this modal's confirm emits `AdminEvent::RequestApply`
    /// straight away, one statement, "one transaction per user-visible
    /// action").
    NewSchema { name: Entity<connections_ui::TextField> },
    /// `cascade` defaults unchecked; the confirm modal's own checkbox
    /// toggles it. Confirm builds `admin_sql::drop_schema(engine, &schema,
    /// cascade)` and — only when `cascade` is set — a red warning line
    /// (design §2) that T4's Apply dialog already renders via
    /// `ApplyDialogState.warning`.
    DropSchema { schema: String, cascade: bool },
}

/// Panel → main.rs (main owns the runner and the confirm dialog — §3-novela:
/// every write still reaches `Connection::execute` only through main.rs's
/// one confirm modal + `run_write_transaction`'s shared guard).
pub enum AdminEvent {
    /// Panel-built labeled SELECTs (panel knows its engine) → main forwards
    /// to `QueryRunner::fetch_admin_catalog`.
    FetchCatalog { queries: Vec<(&'static str, String)> },
    /// Staged statements → main opens the (generalized) Apply confirm
    /// dialog, which shows `display_sql` ('***'-redacted where it matters)
    /// and dispatches through `run_write_transaction` on confirm. `warning`
    /// is T6's red CASCADE line; always `None` from this sub-view.
    RequestApply { statements: Vec<WriteStatement>, warning: Option<String> },
}

pub struct AdminPanel {
    engine: Engine,
    /// Stamped once at open time — `main.rs`'s `open_admin_apply_dialog`
    /// re-checks this against the CURRENTLY active connection before
    /// dispatching (same belt-and-braces posture as the sandbox Apply
    /// flow's `conn_identity` guard, `tabs.rs`'s doc comment on
    /// `ResultTab::conn_identity`).
    conn_identity: String,
    sub_view: AdminSubView,
    loading: bool,
    error: Option<String>,

    roles: Vec<RoleRow>,
    /// pg: `membership`. MSSQL: `db_role_members`.
    memberships: Vec<Membership>,
    /// MSSQL only: `server_role_members` — pg has no server-level role
    /// scope, so this stays empty there.
    server_memberships: Vec<Membership>,
    selected_role: Option<String>,
    membership_edits: MembershipEdits,
    staged_role_actions: Vec<WriteStatement>,

    /// T5: distinct schema names, pushed in by `main.rs::set_schemas`
    /// (`open_fresh_admin_tab` at open time, the active-slot schema fetch on
    /// every subsequent refresh) — feeds the Privileges sub-view's schema
    /// selector.
    schemas: Vec<String>,
    /// T5: the Privileges sub-view's (schema, grantee) scope selection —
    /// both `None` until the user picks each; `fetch_queries_for` only
    /// dispatches once a schema is chosen (the SQL doesn't need a grantee —
    /// `MatrixState::from_catalog` filters client-side — but the matrix
    /// stays empty/unbuilt until both are set, see `apply_catalog`).
    selected_schema: Option<String>,
    selected_grantee: Option<String>,
    /// T5: pg only — parsed out of the `db_acl` batch's `database` column
    /// (design §1: that query is scoped to `current_database()`, so every
    /// row it returns carries the SAME name) since `MatrixState`'s own
    /// shape (design §5's interface spec) has no field for it, and
    /// `MatrixState::to_statements`'s pg-only `GRANT/REVOKE ... ON DATABASE
    /// "…"` statements need the literal name. `None` until the first
    /// Privileges fetch resolves against a live pg connection; MSSQL never
    /// populates this (no `db_acl` label there) and never reads it either.
    current_database: Option<String>,
    matrix: MatrixState,

    /// T6: `sizes_catalog`'s `"databases"` label, parsed via
    /// `parse_db_sizes`.
    db_sizes: Vec<(String, Option<u64>)>,
    /// T6: `sizes_catalog`'s `"schema_sizes"` label, parsed via
    /// `parse_schema_sizes` — schemas of the CURRENT database only.
    schema_sizes: Vec<(String, u64)>,
    /// T6: `sizes_catalog`'s `"current_db_size"` label, parsed via
    /// `current_db_size_label` — the headline's value half.
    current_db_size_label: Option<String>,
    /// T6: which `schema_sizes` row is selected as "Smazat schéma"'s
    /// target — separate from T5's `selected_schema` (the Privileges
    /// sub-view's SCOPE selector, a different concept entirely, and the
    /// two sub-views' selections must survive independently across a
    /// sub-view round-trip).
    selected_size_schema: Option<String>,

    modal: Option<AdminModal>,
    /// A sub-view switch OR a schema/grantee re-selection requested while
    /// the CURRENT sub-view is dirty — renders the "Zahodit neuložené
    /// změny?" prompt instead of proceeding silently (design §2).
    discard_confirm: Option<PendingAdminAction>,

    #[allow(dead_code)] // reserved for future keyboard/escape handling, same posture as SchemaTree's.
    focus_handle: FocusHandle,
    /// UX-polish: focus target for the no-input DropSchema modal —
    /// panel-local twin of AppView's modal_focus_handle (§1.4).
    modal_focus_handle: FocusHandle,
}

impl AdminPanel {
    pub fn new(engine: Engine, conn_identity: String, cx: &mut Context<Self>) -> Self {
        Self {
            engine,
            conn_identity,
            sub_view: AdminSubView::Roles,
            loading: false,
            error: None,
            roles: Vec::new(),
            memberships: Vec::new(),
            server_memberships: Vec::new(),
            selected_role: None,
            membership_edits: MembershipEdits::default(),
            staged_role_actions: Vec::new(),
            schemas: Vec::new(),
            selected_schema: None,
            selected_grantee: None,
            current_database: None,
            matrix: MatrixState::default(),
            db_sizes: Vec::new(),
            schema_sizes: Vec::new(),
            current_db_size_label: None,
            selected_size_schema: None,
            modal: None,
            discard_confirm: None,
            focus_handle: cx.focus_handle(),
            modal_focus_handle: cx.focus_handle(),
        }
    }

    pub fn conn_identity(&self) -> &str {
        &self.conn_identity
    }

    /// T5: see the `schemas` field's doc comment.
    pub fn set_schemas(&mut self, schemas: Vec<String>, cx: &mut Context<Self>) {
        self.schemas = schemas;
        cx.notify();
    }

    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();
    }

    pub fn set_error(&mut self, msg: &str, cx: &mut Context<Self>) {
        self.loading = false;
        self.error = Some(msg.to_string());
        cx.notify();
    }

    /// Routes each labeled result to its parser by label (design §1's
    /// labels — see `admin_sql::roles_catalog`'s doc comment for the exact
    /// per-engine label lists). MSSQL's two principal lists merge into one
    /// `roles` list, deduped by name (`server_principals` sets the
    /// baseline, `db_principals` appends anything not already present) —
    /// the generic `RoleRow` shape doesn't distinguish server- vs.
    /// db-scoped principals beyond that.
    ///
    /// T5: a privileges-catalog batch (`object_acl`/`schema_acl`/`db_acl`
    /// pg, or `object_perms`/`schema_perms` MSSQL) is routed as ONE WHOLE
    /// batch into `MatrixState::from_catalog` — never per-label like the
    /// Roles labels below, since the matrix needs every label together to
    /// build `objects`/`current`/`schema_current`/`db_current` in one pass
    /// (`MatrixState::from_catalog`'s own signature takes the full labeled
    /// slice). `current_database` is parsed out of `db_acl`'s rows here
    /// (see that field's doc comment) since `MatrixState` itself has no
    /// slot for it.
    pub fn apply_catalog(&mut self, rows: Vec<(&'static str, AdminCatalogRows)>, cx: &mut Context<Self>) {
        self.loading = false;
        self.error = None;

        if is_privileges_batch(&rows) {
            if let Some((_, (cols, data))) = rows.iter().find(|(l, _)| *l == "db_acl") {
                if let Some(db_ix) = cols.iter().position(|c| c == "database") {
                    if let Some(name) = data.first().and_then(|r| r.get(db_ix)).cloned().flatten() {
                        self.current_database = Some(name);
                    }
                }
            }
            if let Some(grantee) = self.selected_grantee.clone() {
                self.matrix = MatrixState::from_catalog(self.engine, &grantee, &rows);
            }
            cx.notify();
            return;
        }

        // T6: a sizes-catalog batch is per-label like Roles' own routing
        // (unlike Privileges' whole-batch routing above) — each label maps
        // to its OWN independent field, no cross-label combination needed.
        if is_sizes_batch(&rows) {
            for (label, data) in &rows {
                match *label {
                    "current_db_size" => self.current_db_size_label = current_db_size_label(self.engine, data),
                    "databases" => self.db_sizes = parse_db_sizes(self.engine, data),
                    "schema_sizes" => self.schema_sizes = parse_schema_sizes(self.engine, data),
                    _ => {}
                }
            }
            cx.notify();
            return;
        }

        for (label, data) in rows {
            match label {
                "roles" | "server_principals" => self.roles = parse_roles(&data),
                "db_principals" => {
                    for r in parse_roles(&data) {
                        if !self.roles.iter().any(|existing| existing.name == r.name) {
                            self.roles.push(r);
                        }
                    }
                }
                "membership" | "db_role_members" => self.memberships = parse_memberships(&data, false),
                "server_role_members" => self.server_memberships = parse_memberships(&data, true),
                _ => {}
            }
        }
        cx.notify();
    }

    /// Post-Apply-success: clear staged sets (including T5's `matrix`) +
    /// re-request the active sub-view's catalog (design §2/§3's "one
    /// transaction per user-visible action, then refresh").
    pub fn on_apply_success(&mut self, cx: &mut Context<Self>) {
        self.staged_role_actions.clear();
        self.membership_edits.clear();
        self.matrix.clear();
        self.loading = true;
        cx.notify();
        if let Some(queries) = self.fetch_queries_for(self.sub_view) {
            cx.emit(AdminEvent::FetchCatalog { queries });
        }
    }

    fn fetch_queries_for(&self, sub_view: AdminSubView) -> Option<Vec<(&'static str, String)>> {
        match sub_view {
            AdminSubView::Roles => Some(admin_sql::roles_catalog(self.engine)),
            AdminSubView::Privileges => {
                let schema = self.selected_schema.as_deref()?;
                Some(admin_sql::privileges_catalog(self.engine, schema))
            }
            AdminSubView::Databases => Some(admin_sql::sizes_catalog(self.engine)),
        }
    }

    fn is_dirty(&self) -> bool {
        self.change_count() > 0
    }

    /// The panel's one dirtiness definition — drives BOTH the in-panel
    /// Apply bar ("{n} změn") / sub-nav discard-confirm prompt AND (via
    /// `main.rs`'s `AppView::grid_dirty_change_count`/`render_tab_strip`)
    /// the tab-strip's "✕" close guard and " •" dirty indicator. `pub` so
    /// `main.rs` can read it without duplicating what "dirty" means for
    /// this panel a second time. Delegates to `combined_change_count` (the
    /// free-function, GPUI-free half of this same arithmetic) so the exact
    /// formula is directly unit-testable without an `AdminPanel` instance —
    /// this codebase has no established "construct a GPUI entity in a test"
    /// precedent (see e.g. `admin_open_decision`/`conn_identity_matches` in
    /// `main.rs`, which keep their pure decisions free of `Context`/`cx`
    /// too). T5 folds `MatrixState::change_count` into the SAME formula
    /// rather than adding a second dirtiness definition.
    pub fn change_count(&self) -> usize {
        combined_change_count(self.staged_role_actions.len(), &self.membership_edits, self.matrix.change_count())
    }

    fn is_member(&self, role: &str, member: &str, server_role: bool) -> bool {
        let list = if server_role { &self.server_memberships } else { &self.memberships };
        list.iter().any(|m| m.role == role && m.member == member)
    }

    /// Sub-nav click: switches immediately when clean, else opens the
    /// discard-confirm prompt (design §2).
    fn request_sub_view(&mut self, target: AdminSubView, cx: &mut Context<Self>) {
        if target == self.sub_view {
            return;
        }
        if self.is_dirty() {
            self.discard_confirm = Some(PendingAdminAction::SwitchSubView(target));
            cx.notify();
            return;
        }
        self.switch_sub_view(target, cx);
    }

    fn switch_sub_view(&mut self, target: AdminSubView, cx: &mut Context<Self>) {
        self.sub_view = target;
        self.staged_role_actions.clear();
        self.membership_edits.clear();
        self.selected_role = None;
        // T5: leaving/entering Privileges always drops the matrix's staged
        // AND committed state — a fresh fetch (below, once schema+grantee
        // are set) repopulates `current`; carrying stale committed rows
        // across an unrelated sub-view round-trip would risk showing data
        // that's since drifted.
        self.matrix = MatrixState::default();
        // T6: same "drop stale display data on a sub-view round-trip"
        // posture — no staged diff here (schema DDL is direct-to-confirm,
        // never locally staged), just display fields that a fresh fetch
        // repopulates.
        self.db_sizes.clear();
        self.schema_sizes.clear();
        self.current_db_size_label = None;
        self.selected_size_schema = None;
        self.loading = true;
        cx.notify();
        if let Some(queries) = self.fetch_queries_for(target) {
            cx.emit(AdminEvent::FetchCatalog { queries });
        } else {
            self.loading = false;
        }
    }

    /// T5: schema selector click — same dirty-guard shape as
    /// `request_sub_view`. A no-op when re-selecting the already-selected
    /// schema.
    fn request_select_schema(&mut self, schema: String, cx: &mut Context<Self>) {
        if self.selected_schema.as_deref() == Some(schema.as_str()) {
            return;
        }
        if self.is_dirty() {
            self.discard_confirm = Some(PendingAdminAction::SelectSchema(schema));
            cx.notify();
            return;
        }
        self.apply_select_schema(schema, cx);
    }

    /// T5: grantee selector click — same shape as `request_select_schema`.
    fn request_select_grantee(&mut self, grantee: String, cx: &mut Context<Self>) {
        if self.selected_grantee.as_deref() == Some(grantee.as_str()) {
            return;
        }
        if self.is_dirty() {
            self.discard_confirm = Some(PendingAdminAction::SelectGrantee(grantee));
            cx.notify();
            return;
        }
        self.apply_select_grantee(grantee, cx);
    }

    fn apply_select_schema(&mut self, schema: String, cx: &mut Context<Self>) {
        self.selected_schema = Some(schema);
        self.refetch_privileges(cx);
    }

    fn apply_select_grantee(&mut self, grantee: String, cx: &mut Context<Self>) {
        self.selected_grantee = Some(grantee);
        self.refetch_privileges(cx);
    }

    /// T5: drops the matrix's staged+committed state and re-dispatches
    /// `privileges_catalog(engine, schema)` — the query itself doesn't take
    /// a grantee (`MatrixState::from_catalog` filters client-side, see its
    /// doc comment), so a grantee-only change re-runs the SAME query rather
    /// than re-filtering cached rows; simpler and always-correct at the
    /// cost of one extra round-trip, acceptable for this phase. A no-op
    /// (stays `loading = false`) until a schema is actually selected.
    fn refetch_privileges(&mut self, cx: &mut Context<Self>) {
        self.matrix = MatrixState::default();
        self.loading = true;
        cx.notify();
        if let Some(queries) = self.fetch_queries_for(AdminSubView::Privileges) {
            cx.emit(AdminEvent::FetchCatalog { queries });
        } else {
            self.loading = false;
        }
    }

    fn discard_confirm_yes(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.discard_confirm.take() else { return };
        match action {
            PendingAdminAction::SwitchSubView(target) => self.switch_sub_view(target, cx),
            PendingAdminAction::SelectSchema(schema) => self.apply_select_schema(schema, cx),
            PendingAdminAction::SelectGrantee(grantee) => self.apply_select_grantee(grantee, cx),
        }
    }

    fn discard_confirm_no(&mut self, cx: &mut Context<Self>) {
        self.discard_confirm = None;
        cx.notify();
    }

    fn select_role(&mut self, name: String, cx: &mut Context<Self>) {
        self.selected_role = Some(name);
        cx.notify();
    }

    fn open_new_role_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let name = cx.new(|cx| connections_ui::TextField::form_field(cx, "název role", false));
        let password = cx.new(|cx| connections_ui::TextField::form_field(cx, "heslo", true));
        let focus = name.focus_handle(cx);
        self.modal = Some(AdminModal::NewRole {
            name,
            password,
            login: true,
            superuser: false,
            createdb: false,
            createrole: false,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn open_change_password_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(role) = self.selected_role.clone() else { return };
        if self.modal.is_some() {
            return;
        }
        let password = cx.new(|cx| connections_ui::TextField::form_field(cx, "nové heslo", true));
        let focus = password.focus_handle(cx);
        self.modal = Some(AdminModal::ChangePassword { role, password });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        cx.notify();
    }

    /// UX-polish sweep #9: called from `AppView::on_cancel_query`'s new
    /// `TabContent::Admin` arm — the SAME "the unscoped root Esc binding
    /// is the mechanism" shape as `ResultGrid::close_overlay_if_open`.
    /// Discard-confirm first (Esc = „Zpět", never „Zahodit" — the G5
    /// rule), then the modal. Returns true when Esc was CONSUMED — which
    /// includes the refused password case, so Esc against a typed
    /// password does nothing at all rather than cancelling a query
    /// underneath (mirrors the app-modal `closable` match's `return`).
    pub fn close_overlay_if_open(&mut self, cx: &mut Context<Self>) -> bool {
        if self.discard_confirm.is_some() {
            self.discard_confirm_no(cx);
            return true;
        }
        if let Some(modal) = &self.modal {
            let (has_password, password_empty) = match modal {
                AdminModal::NewRole { password, .. }
                | AdminModal::ChangePassword { password, .. } => {
                    (true, password.read(cx).text().is_empty())
                }
                AdminModal::NewSchema { .. } | AdminModal::DropSchema { .. } => (true, true),
            };
            if admin_esc_closable(has_password, password_empty) {
                self.close_modal(cx);
            }
            return true;
        }
        false
    }

    /// UX-polish §1.2 (admin rows): Enter = the confirm button. NewRole/
    /// ChangePassword stage a WriteStatement (execution still goes through
    /// the apply bar → apply dialog gate); NewSchema emits RequestApply →
    /// opens the apply dialog (a second explicit gate). DropSchema is a
    /// DELIBERATE handled no-op (§3-novela: destructive intent — the pause
    /// is the point, CASCADE warning) — do not wire it.
    fn on_modal_confirm(
        &mut self,
        _: &connections_ui::ModalConfirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.modal {
            Some(AdminModal::NewRole { .. }) => self.confirm_new_role(cx),
            Some(AdminModal::ChangePassword { .. }) => self.confirm_change_password(cx),
            Some(AdminModal::NewSchema { .. }) => self.confirm_new_schema(cx),
            Some(AdminModal::DropSchema { .. }) | None => {}
        }
    }

    fn on_modal_focus_next(
        &mut self,
        _: &connections_ui::ModalFocusNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
    }

    fn on_modal_focus_prev(
        &mut self,
        _: &connections_ui::ModalFocusPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }

    fn toggle_new_role_flag(&mut self, flag: RoleFlagKind, cx: &mut Context<Self>) {
        if let Some(AdminModal::NewRole { login, superuser, createdb, createrole, .. }) = &mut self.modal {
            match flag {
                RoleFlagKind::Login => *login = !*login,
                RoleFlagKind::Superuser => *superuser = !*superuser,
                RoleFlagKind::Createdb => *createdb = !*createdb,
                RoleFlagKind::Createrole => *createrole = !*createrole,
            }
            cx.notify();
        }
    }

    /// CURATION item 4: the modal-local password lives in `Zeroizing`,
    /// derefs into the builder, and is overwritten on drop at the end of
    /// this function — only the staged `WriteStatement`'s `exec_sql` keeps
    /// the value, and that dies with the `Vec` when the eventual
    /// transaction future completes (never cached in panel state beyond
    /// this call).
    fn confirm_new_role(&mut self, cx: &mut Context<Self>) {
        let Some(AdminModal::NewRole { name, password, login, superuser, createdb, createrole }) =
            &self.modal
        else {
            return;
        };
        let role_name = name.read(cx).text();
        if role_name.trim().is_empty() {
            return;
        }
        let password: zeroize::Zeroizing<String> = zeroize::Zeroizing::new(password.read(cx).text());
        let flags = admin_sql::RoleFlags {
            login: *login,
            superuser: *superuser,
            createdb: *createdb,
            createrole: *createrole,
        };
        self.staged_role_actions.extend(admin_sql::create_role(
            self.engine,
            role_name.trim(),
            &password,
            &flags,
        ));
        self.modal = None;
        cx.notify();
    }

    fn confirm_change_password(&mut self, cx: &mut Context<Self>) {
        let Some(AdminModal::ChangePassword { role, password }) = &self.modal else { return };
        let role = role.clone();
        let password: zeroize::Zeroizing<String> = zeroize::Zeroizing::new(password.read(cx).text());
        self.staged_role_actions.extend(admin_sql::alter_password(self.engine, &role, &password));
        self.modal = None;
        cx.notify();
    }

    /// "Smazat roli" — stages `drop_role` directly, no modal (the outer
    /// confirm dialog IS the confirmation).
    fn stage_drop_role(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.selected_role.take() else { return };
        self.staged_role_actions.extend(admin_sql::drop_role(self.engine, &name));
        cx.notify();
    }

    /// T6: "Smazat schéma"'s row-click target selector.
    fn select_size_schema(&mut self, name: String, cx: &mut Context<Self>) {
        self.selected_size_schema = Some(name);
        cx.notify();
    }

    /// T6: "Nové schéma…" — opens the name-entry modal.
    fn open_new_schema_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let name = cx.new(|cx| connections_ui::TextField::form_field(cx, "název schématu", false));
        let focus = name.focus_handle(cx);
        self.modal = Some(AdminModal::NewSchema { name });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// T6: "Smazat schéma" — opens the CASCADE-checkbox confirm modal for
    /// `self.selected_size_schema`. No `TextField` here — UX-polish sweep
    /// #9 gives it `modal_focus_handle` instead (panel-local twin of
    /// `AppView`'s §1.4 mechanism) so it still holds keyboard focus and
    /// stray typing can't reach whatever was focused underneath.
    fn open_drop_schema_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = self.selected_size_schema.clone() else { return };
        if self.modal.is_some() {
            return;
        }
        self.modal = Some(AdminModal::DropSchema { schema, cascade: false });
        window.focus(&self.modal_focus_handle, cx);
        cx.notify();
    }

    fn toggle_drop_schema_cascade(&mut self, cx: &mut Context<Self>) {
        if let Some(AdminModal::DropSchema { cascade, .. }) = &mut self.modal {
            *cascade = !*cascade;
            cx.notify();
        }
    }

    /// T6: direct-to-confirm (design §2, "Resolved design ambiguities"
    /// item 6) — unlike the Roles modals, this does NOT stage into
    /// `staged_role_actions`; it emits `RequestApply` immediately, exactly
    /// one `CREATE SCHEMA` statement, matching "one transaction per
    /// user-visible action".
    fn confirm_new_schema(&mut self, cx: &mut Context<Self>) {
        let Some(AdminModal::NewSchema { name }) = &self.modal else { return };
        let schema_name = name.read(cx).text();
        if schema_name.trim().is_empty() {
            return;
        }
        let statements = admin_sql::create_schema(self.engine, schema_name.trim());
        self.modal = None;
        if statements.is_empty() {
            cx.notify();
            return;
        }
        cx.emit(AdminEvent::RequestApply { statements, warning: None });
        cx.notify();
    }

    /// T6: direct-to-confirm, same shape as `confirm_new_schema` — the
    /// CASCADE checkbox becomes both `drop_schema`'s `cascade` argument AND
    /// (when checked) the dialog's red warning line (design §2: "unchecked
    /// plain DROP SCHEMA failing on a non-empty schema surfaces the
    /// engine's own error in the dialog — let the server say no").
    fn confirm_drop_schema(&mut self, cx: &mut Context<Self>) {
        let Some(AdminModal::DropSchema { schema, cascade }) = &self.modal else { return };
        let schema = schema.clone();
        let cascade = *cascade;
        let statements = admin_sql::drop_schema(self.engine, &schema, cascade);
        self.modal = None;
        self.selected_size_schema = None;
        if statements.is_empty() {
            cx.notify();
            return;
        }
        let warning = cascade.then(|| "tato akce je nevratná a smaže i obsah schématu".to_string());
        cx.emit(AdminEvent::RequestApply { statements, warning });
        cx.notify();
    }

    /// The Apply bar's "Zahodit" — clears EVERY sub-view's staged state
    /// (only the active one is ever actually non-empty, per
    /// `switch_sub_view`'s own clearing, but this stays defensive of that
    /// invariant rather than relying on it).
    fn discard_staged(&mut self, cx: &mut Context<Self>) {
        self.staged_role_actions.clear();
        self.membership_edits.clear();
        self.matrix.clear();
        cx.notify();
    }

    /// Builds `AdminEvent::RequestApply`'s statements from whichever
    /// sub-view is active. T5's Privileges arm surfaces a `MatrixState::
    /// to_statements` `Err` (unreachable via the UI cycles — the
    /// errors-are-values backstop) in `self.error` rather than panicking,
    /// exactly like the plan's Grounding specifies.
    fn request_apply(&mut self, cx: &mut Context<Self>) {
        match self.sub_view {
            AdminSubView::Roles => {
                let mut statements = self.staged_role_actions.clone();
                statements.extend(self.membership_edits.to_statements(self.engine));
                if statements.is_empty() {
                    return;
                }
                cx.emit(AdminEvent::RequestApply { statements, warning: None });
            }
            AdminSubView::Privileges => {
                let (Some(schema), Some(grantee)) =
                    (self.selected_schema.clone(), self.selected_grantee.clone())
                else {
                    return;
                };
                let database = self.current_database.clone().unwrap_or_default();
                match self.matrix.to_statements(self.engine, &schema, &grantee, &database) {
                    Ok(statements) => {
                        if statements.is_empty() {
                            return;
                        }
                        cx.emit(AdminEvent::RequestApply { statements, warning: None });
                    }
                    Err(e) => {
                        self.error = Some(e);
                        cx.notify();
                    }
                }
            }
            AdminSubView::Databases => {} // T6
        }
    }

    // -------------------------------------------------------------
    // Render helpers.
    // -------------------------------------------------------------

    fn render_sub_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let tabs = [
            (AdminSubView::Roles, "Role a členství"),
            (AdminSubView::Privileges, "Oprávnění"),
            (AdminSubView::Databases, "Databáze a schémata"),
        ];
        let mut row = div().flex().flex_row().gap_2().px_2().py_1().bg(theme.bg_app);
        for (view, label) in tabs {
            let active = view == self.sub_view;
            row = row.child(
                div()
                    .id(format!("admin-subnav-{label}"))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(active, |d| d.bg(theme.bg_hover))
                    .text_color(if active { theme.text_primary } else { theme.text_muted })
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.request_sub_view(view, cx);
                    })),
            );
        }
        row
    }

    fn render_roles_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let roles = self.roles.clone();
        let selected = self.selected_role.clone();

        // sweep: one-off darker divider (0x313244) folded into the standard border role
        let mut list = div().flex().flex_col().w(px(220.)).overflow_hidden().border_r_1().border_color(theme.border);
        for r in &roles {
            let is_selected = selected.as_deref() == Some(r.name.as_str());
            let name_for_click = r.name.clone();
            list = list.child(
                div()
                    .id(format!("admin-role-row-{}", r.name))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .when(is_selected, |d| d.bg(theme.bg_selected))
                    .hover(|s| s.bg(theme.bg_hover))
                    .text_color(theme.text_primary)
                    .child(r.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.select_role(name_for_click.clone(), cx);
                    })),
            );
        }

        let mut detail = div().flex().flex_col().flex_1().p_2().gap_2().text_color(theme.text_primary);
        if let Some(sel) = &selected {
            if let Some(row) = roles.iter().find(|r| &r.name == sel) {
                for (k, v) in &row.detail {
                    detail = detail.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(div().w(px(160.)).text_color(theme.text_muted).child(k.clone()))
                            .child(div().child(v.clone())),
                    );
                }
            }

            // "Členem v" — one checkbox per known role name, plus (MSSQL)
            // a second heading for server-scoped roles.
            detail = detail.child(div().mt_2().text_color(theme.text_muted).child("Členem v"));
            let mut member_list = div().flex().flex_col();
            for (ix, r) in roles.iter().filter(|r| &r.name != sel).enumerate() {
                member_list = member_list.child(self.render_membership_checkbox(
                    ix,
                    r.name.clone(),
                    sel.clone(),
                    false,
                    cx,
                ));
            }
            detail = detail.child(member_list);

            if self.engine == Engine::Mssql {
                let server_roles: Vec<String> =
                    self.server_memberships.iter().map(|m| m.role.clone()).collect();
                if !server_roles.is_empty() {
                    detail = detail.child(div().mt_2().text_color(theme.text_muted).child("Členem v (server)"));
                    let mut srv_list = div().flex().flex_col();
                    for (ix, role_name) in server_roles.into_iter().enumerate() {
                        srv_list = srv_list.child(self.render_membership_checkbox(
                            ix,
                            role_name,
                            sel.clone(),
                            true,
                            cx,
                        ));
                    }
                    detail = detail.child(srv_list);
                }
            }
        } else {
            detail = detail.child(div().text_color(theme.text_disabled).child("Vyberte roli vlevo."));
        }

        let buttons = div()
            .flex()
            .flex_row()
            .gap_2()
            .p_2()
            .child(
                div()
                    .id("admin-new-role")
                    .cursor_pointer()
                    .bg(theme.bg_hover)
                    .px_2()
                    .rounded_md()
                    .text_color(theme.text_primary)
                    .child("Nová role…")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_new_role_modal(window, cx);
                    })),
            )
            .child(
                div()
                    .id("admin-drop-role")
                    .when(selected.is_some(), |d| d.cursor_pointer())
                    .bg(theme.bg_hover)
                    .px_2()
                    .rounded_md()
                    .text_color(if selected.is_some() { theme.danger } else { theme.text_disabled })
                    .child("Smazat roli")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.stage_drop_role(cx);
                    })),
            )
            .child(
                div()
                    .id("admin-change-password")
                    .when(selected.is_some(), |d| d.cursor_pointer())
                    .bg(theme.bg_hover)
                    .px_2()
                    .rounded_md()
                    .text_color(if selected.is_some() { theme.text_primary } else { theme.text_disabled })
                    .child("Změnit heslo…")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_change_password_modal(window, cx);
                    })),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(buttons)
            .child(div().flex().flex_row().flex_1().overflow_hidden().child(list).child(detail))
            .into_any_element()
    }

    /// T5: "Oprávnění" — schema/grantee selector chips (bounded, small
    /// lists — same plain-loop posture `render_roles_body`'s own role list
    /// already takes, unlike the object grid below), the fixed
    /// schema/db-privilege checkbox row (design §2), and the object×
    /// privilege grid. The grid is the one part of this sub-view that CAN
    /// be large (a schema can hold hundreds of tables) — rendered through
    /// `uniform_list`, same virtualization `schema_tree.rs` uses for its
    /// row list, never a plain per-object `.child()` loop.
    fn render_privileges_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let schemas = self.schemas.clone();
        let roles = self.roles.clone();
        let selected_schema = self.selected_schema.clone();
        let selected_grantee = self.selected_grantee.clone();

        let mut schema_row = div().flex().flex_row().items_center().flex_wrap().gap_1().px_2().py_1();
        schema_row = schema_row.child(div().text_color(theme.text_muted).child("Schéma:"));
        for (ix, s) in schemas.iter().enumerate() {
            let is_sel = selected_schema.as_deref() == Some(s.as_str());
            let s_for_click = s.clone();
            schema_row = schema_row.child(
                div()
                    .id(("admin-priv-schema-chip", ix))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(is_sel, |d| d.bg(theme.bg_selected))
                    .hover(|s| s.bg(theme.bg_hover))
                    .text_color(theme.text_primary)
                    .child(s.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.request_select_schema(s_for_click.clone(), cx);
                    })),
            );
        }

        let mut grantee_row = div().flex().flex_row().items_center().flex_wrap().gap_1().px_2().py_1();
        grantee_row = grantee_row.child(div().text_color(theme.text_muted).child("Role:"));
        for (ix, r) in roles.iter().enumerate() {
            let is_sel = selected_grantee.as_deref() == Some(r.name.as_str());
            let r_for_click = r.name.clone();
            grantee_row = grantee_row.child(
                div()
                    .id(("admin-priv-grantee-chip", ix))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(is_sel, |d| d.bg(theme.bg_selected))
                    .hover(|s| s.bg(theme.bg_hover))
                    .text_color(theme.text_primary)
                    .child(r.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.request_select_grantee(r_for_click.clone(), cx);
                    })),
            );
        }

        let mut root = div().flex().flex_col().flex_1().overflow_hidden();
        root = root.child(schema_row).child(grantee_row);

        let Some(schema) = selected_schema.clone() else {
            return root
                .child(div().flex_1().p_2().text_color(theme.text_disabled).child("Vyberte schéma a roli."))
                .into_any_element();
        };
        if selected_grantee.is_none() {
            return root
                .child(div().flex_1().p_2().text_color(theme.text_disabled).child("Vyberte roli."))
                .into_any_element();
        }

        // Fixed schema/db-privilege checkbox row (design §2) — SCHEMA_PRIVS
        // always, PG_DATABASE_PRIVS only for Postgres (design §2: the
        // db-level row is pg only). One clickable cell per privilege,
        // showing the SAME ✓/✗/empty glyph + yellow staged-tint convention
        // the object grid below uses.
        let engine = self.engine;
        let mut scope_row =
            div().flex().flex_row().items_center().flex_wrap().gap_2().px_2().py_1().bg(theme.bg_app);
        scope_row = scope_row.child(div().text_color(theme.text_muted).child(format!("Schéma \"{schema}\":")));
        for (ix, &priv_name) in admin_sql::SCHEMA_PRIVS.iter().enumerate() {
            let committed = self.matrix.schema_current.get(priv_name).copied().unwrap_or(CellState::NotSet);
            let state = self.matrix.schema_staged.get(priv_name).copied().unwrap_or(committed);
            let staged = state != committed;
            let glyph = privilege_glyph(state);
            scope_row = scope_row.child(
                div()
                    .id(("admin-priv-schema-cell", ix))
                    .cursor_pointer()
                    .px_1()
                    .when(staged, |d| d.bg(theme.diff_staged_bg))
                    .text_color(theme.text_primary)
                    .child(format!("{priv_name} {glyph}"))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.matrix.click_schema_cell(engine, priv_name);
                        cx.notify();
                    })),
            );
        }
        if self.engine == Engine::Postgres {
            scope_row = scope_row.child(div().text_color(theme.text_muted).child("Databáze:"));
            for (ix, &priv_name) in admin_sql::PG_DATABASE_PRIVS.iter().enumerate() {
                let committed = self.matrix.db_current.get(priv_name).copied().unwrap_or(CellState::NotSet);
                let state = self.matrix.db_staged.get(priv_name).copied().unwrap_or(committed);
                let staged = state != committed;
                let glyph = privilege_glyph(state);
                scope_row = scope_row.child(
                    div()
                        .id(("admin-priv-db-cell", ix))
                        .cursor_pointer()
                        .px_1()
                        .when(staged, |d| d.bg(theme.diff_staged_bg))
                        .text_color(theme.text_primary)
                        .child(format!("{priv_name} {glyph}"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.matrix.click_db_cell(priv_name);
                            cx.notify();
                        })),
                );
            }
        }
        root = root.child(scope_row);

        let objects = self.matrix.objects.clone();
        let n_objects = objects.len();
        let priv_columns: &'static [&'static str] = match self.engine {
            Engine::Postgres => admin_sql::PG_TABLE_PRIVS,
            Engine::Mssql => admin_sql::MSSQL_TABLE_PRIVS,
            // G16: Duckdb joins Sqlite — admin Hidden, no columns.
            Engine::Sqlite | Engine::Duckdb => &[],
        };

        if n_objects == 0 {
            return root
                .child(div().flex_1().p_2().text_color(theme.text_disabled).child("Žádné objekty v tomto schématu."))
                .into_any_element();
        }

        let mut header = div().flex().flex_row().px_2().py_1().bg(theme.bg_app).text_color(theme.text_muted);
        header = header.child(div().w(px(220.)).child("Objekt"));
        for &p in priv_columns {
            header = header.child(div().w(px(80.)).child(p));
        }
        root = root.child(header);

        let n_cols = priv_columns.len();
        root = root.child(
            uniform_list(
                "admin-priv-object-rows",
                n_objects,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let mut items = Vec::with_capacity(range.len());
                    for ix in range {
                        let object = objects[ix].clone();
                        let mut row = div()
                            .id(("admin-priv-object-row", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .px_2()
                            .py_1()
                            .text_color(theme.text_primary)
                            .hover(|s| s.bg(theme.bg_hover));
                        row = row.child(div().w(px(220.)).overflow_hidden().child(object.clone()));
                        for (col_ix, &priv_name) in priv_columns.iter().enumerate() {
                            let key = (object.clone(), priv_name.to_string());
                            let state = this.matrix.effective(&object, priv_name);
                            let committed = this.matrix.current.get(&key).copied().unwrap_or(CellState::NotSet);
                            let staged = state != committed;
                            let glyph = privilege_glyph(state);
                            let object_for_click = object.clone();
                            // `ix * n_cols + col_ix`: a bijective (row, col)
                            // -> single-usize encoding, the same
                            // collision-safe `(&'static str, usize)` id
                            // shape as everywhere else in this file — a
                            // per-row id alone would collide across this
                            // row's own N privilege columns.
                            let cell_ix = ix * n_cols + col_ix;
                            row = row.child(
                                div()
                                    .id(("admin-priv-cell", cell_ix))
                                    .w(px(80.))
                                    .cursor_pointer()
                                    .when(staged, |d| d.bg(theme.diff_staged_bg))
                                    .child(glyph)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                        this.matrix.click_cell(engine, &object_for_click, priv_name);
                                        cx.notify();
                                    })),
                            );
                        }
                        items.push(row);
                    }
                    items
                }),
            )
            .flex_1(),
        );

        root.into_any_element()
    }

    /// T6: "Databáze a schémata" — a headline size line, then TWO
    /// read-only bar lists (databases; the current database's schemas),
    /// then the direct-to-confirm "Nové schéma…"/"Smazat schéma" buttons.
    /// Both lists are plain loops, NOT `uniform_list`: unlike the
    /// Privileges object grid (which can hold hundreds of tables), a
    /// server's database count and a database's schema count are both
    /// small in practice — same bounded-but-unvirtualized posture
    /// `render_roles_body`'s role list and `render_privileges_body`'s
    /// schema/grantee chip rows already take. `CREATE DATABASE`/
    /// `DROP DATABASE` have deliberately NO UI anywhere in this function
    /// (design §3's transaction-block landmine — not silently
    /// reintroduced).
    fn render_databases_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let headline = match &self.current_db_size_label {
            Some(label) => format!("Aktuální databáze: {label}"),
            None => "Aktuální databáze: …".to_string(),
        };

        let db_sizes = self.db_sizes.clone();
        let max_db_bytes = db_sizes.iter().filter_map(|(_, b)| *b).max().unwrap_or(0);
        let mut db_list = div().flex().flex_col().gap_1().px_2().py_1();
        for (ix, (name, bytes)) in db_sizes.iter().enumerate() {
            let mut row = div().id(("admin-db-size-row", ix)).flex().flex_row().items_center().gap_2();
            row = row.child(div().w(px(180.)).overflow_hidden().text_color(theme.text_primary).child(name.clone()));
            row = match bytes {
                Some(b) => row
                    .child(render_size_bar(bar_fraction(*b, max_db_bytes), &theme))
                    .child(div().text_color(theme.text_muted).child(format_bytes(*b))),
                None => row.child(div().text_color(theme.text_disabled).child("—")),
            };
            db_list = db_list.child(row);
        }

        let schema_sizes = self.schema_sizes.clone();
        let selected_size_schema = self.selected_size_schema.clone();
        let max_schema_bytes = schema_sizes.iter().map(|(_, b)| *b).max().unwrap_or(0);
        let mut schema_list = div().flex().flex_col().gap_1().px_2().py_1();
        for (ix, (name, bytes)) in schema_sizes.iter().enumerate() {
            let is_sel = selected_size_schema.as_deref() == Some(name.as_str());
            let name_for_click = name.clone();
            let mut row = div()
                .id(("admin-schema-size-row", ix))
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .when(is_sel, |d| d.bg(theme.bg_selected))
                .hover(|s| s.bg(theme.bg_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.select_size_schema(name_for_click.clone(), cx);
                }));
            row = row.child(div().w(px(180.)).overflow_hidden().text_color(theme.text_primary).child(name.clone()));
            row = row
                .child(render_size_bar(bar_fraction(*bytes, max_schema_bytes), &theme))
                .child(div().text_color(theme.text_muted).child(format_bytes(*bytes)));
            schema_list = schema_list.child(row);
        }

        let drop_enabled = selected_size_schema.is_some();
        let buttons = div()
            .flex()
            .flex_row()
            .gap_2()
            .p_2()
            .child(
                div()
                    .id("admin-new-schema")
                    .cursor_pointer()
                    .bg(theme.bg_hover)
                    .px_2()
                    .rounded_md()
                    .text_color(theme.text_primary)
                    .child("Nové schéma…")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_new_schema_modal(window, cx);
                    })),
            )
            .child(
                div()
                    .id("admin-drop-schema")
                    .when(drop_enabled, |d| d.cursor_pointer())
                    .bg(theme.bg_hover)
                    .px_2()
                    .rounded_md()
                    .text_color(if drop_enabled { theme.danger } else { theme.text_disabled })
                    .child("Smazat schéma")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_drop_schema_modal(window, cx);
                    })),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .child(div().px_2().py_1().text_color(theme.text_primary).child(headline))
            .child(buttons)
            .child(div().px_2().text_color(theme.text_muted).child("Databáze"))
            .child(db_list)
            .child(div().px_2().text_color(theme.text_muted).child("Schémata"))
            .child(schema_list)
            .into_any_element()
    }

    /// `ix` is this row's position within its OWN list (`member_list` or
    /// `srv_list` in `render_roles_body`) — combined with a per-list
    /// literal prefix, that's the same collision-safe `(&'static str,
    /// usize)` id shape `schema_tree.rs`'s `("tree-row", ix)` uses.
    /// Interpolating `role`/`member` strings directly into the id (the
    /// prior shape) was collision-prone: role "a-b" + member "c" and role
    /// "a" + member "b-c" produced the identical id string.
    fn render_membership_checkbox(
        &self,
        ix: usize,
        role: String,
        member: String,
        server_role: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = *cx.theme();
        let currently = self.is_member(&role, &member, server_role);
        let checked = self.membership_edits.is_checked(&role, &member, server_role, currently);
        let staged = checked != currently;
        let mark = if checked { "☑" } else { "☐" };
        let role_for_click = role.clone();
        let member_for_click = member.clone();
        let id_prefix = if server_role { "admin-membership-srv" } else { "admin-membership-db" };
        div()
            .id((id_prefix, ix))
            .cursor_pointer()
            .px_1()
            .flex()
            .flex_row()
            .gap_1()
            .when(staged, |d| d.bg(theme.diff_staged_bg))
            .text_color(theme.text_primary)
            .child(mark)
            .child(role)
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                let currently = this.is_member(&role_for_click, &member_for_click, server_role);
                this.membership_edits.toggle(&role_for_click, &member_for_click, server_role, currently);
                cx.notify();
            }))
    }

    fn render_apply_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let n = self.change_count();
        if n == 0 {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(theme.bg_app)
                .text_color(theme.warn)
                .child(format!("{n} změn"))
                .child(
                    div()
                        .id("admin-apply")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .px_2()
                        .rounded_md()
                        .text_color(theme.success)
                        .child("Aplikovat")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.request_apply(cx))),
                )
                .child(
                    div()
                        .id("admin-discard")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .px_2()
                        .rounded_md()
                        .text_color(theme.text_primary)
                        .child("Zahodit")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.discard_staged(cx))),
                )
                .into_any_element(),
        )
    }

    fn render_modal_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let modal = self.modal.as_ref()?;
        let panel = match modal {
            AdminModal::NewRole { name, password, login, superuser, createdb, createrole } => {
                let flags = [
                    (RoleFlagKind::Login, "LOGIN", *login),
                    (RoleFlagKind::Superuser, "SUPERUSER", *superuser),
                    (RoleFlagKind::Createdb, "CREATEDB", *createdb),
                    (RoleFlagKind::Createrole, "CREATEROLE", *createrole),
                ];
                let mut flags_row = div().flex().flex_row().gap_2();
                for (flag, label, on) in flags {
                    let mark = if on { "☑" } else { "☐" };
                    flags_row = flags_row.child(
                        div()
                            .id(format!("admin-role-flag-{label}"))
                            .cursor_pointer()
                            .text_color(theme.text_primary)
                            .child(format!("{mark} {label}"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.toggle_new_role_flag(flag, cx);
                            })),
                    );
                }
                div()
                    .id("admin-modal-new-role")
                    .w(px(360.))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .flex()
                    .flex_col()
                    .p_2()
                    .gap_2()
                    .text_color(theme.text_primary)
                    .child("Nová role")
                    .child(name.clone())
                    .child(password.clone())
                    .child(flags_row)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("admin-modal-confirm")
                                    .cursor_pointer()
                                    .bg(theme.bg_hover)
                                    .px_2()
                                    .rounded_md()
                                    .text_color(theme.success)
                                    .child("Vytvořit")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        this.confirm_new_role(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("admin-modal-cancel")
                                    .cursor_pointer()
                                    .bg(theme.bg_hover)
                                    .px_2()
                                    .rounded_md()
                                    .text_color(theme.text_primary)
                                    .child("Zrušit")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        this.close_modal(cx);
                                    })),
                            ),
                    )
            }
            AdminModal::ChangePassword { role, password } => div()
                .id("admin-modal-change-password")
                .w(px(360.))
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .flex()
                .flex_col()
                .p_2()
                .gap_2()
                .text_color(theme.text_primary)
                .child(format!("Změnit heslo — {role}"))
                .child(password.clone())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(
                            div()
                                .id("admin-modal-confirm")
                                .cursor_pointer()
                                .bg(theme.bg_hover)
                                .px_2()
                                .rounded_md()
                                .text_color(theme.success)
                                .child("Změnit")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.confirm_change_password(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("admin-modal-cancel")
                                .cursor_pointer()
                                .bg(theme.bg_hover)
                                .px_2()
                                .rounded_md()
                                .text_color(theme.text_primary)
                                .child("Zrušit")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.close_modal(cx);
                                })),
                        ),
                ),
            AdminModal::NewSchema { name } => div()
                .id("admin-modal-new-schema")
                .w(px(360.))
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .flex()
                .flex_col()
                .p_2()
                .gap_2()
                .text_color(theme.text_primary)
                .child("Nové schéma")
                .child(name.clone())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(
                            div()
                                .id("admin-modal-confirm")
                                .cursor_pointer()
                                .bg(theme.bg_hover)
                                .px_2()
                                .rounded_md()
                                .text_color(theme.success)
                                .child("Vytvořit")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.confirm_new_schema(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("admin-modal-cancel")
                                .cursor_pointer()
                                .bg(theme.bg_hover)
                                .px_2()
                                .rounded_md()
                                .text_color(theme.text_primary)
                                .child("Zrušit")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.close_modal(cx);
                                })),
                        ),
                ),
            AdminModal::DropSchema { schema, cascade } => {
                let mark = if *cascade { "☑" } else { "☐" };
                div()
                    .id("admin-modal-drop-schema")
                    .w(px(420.))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .flex()
                    .flex_col()
                    .p_2()
                    .gap_2()
                    .text_color(theme.text_primary)
                    .child(format!("Smazat schéma — {schema}"))
                    .child(
                        div()
                            .id("admin-drop-schema-cascade")
                            .cursor_pointer()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .text_color(theme.warn)
                            .child(format!("{mark} včetně CASCADE (smaže i obsah schématu)"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.toggle_drop_schema_cascade(cx);
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("admin-modal-confirm")
                                    .cursor_pointer()
                                    .bg(theme.bg_hover)
                                    .px_2()
                                    .rounded_md()
                                    .text_color(theme.danger)
                                    .child("Smazat")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        this.confirm_drop_schema(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("admin-modal-cancel")
                                    .cursor_pointer()
                                    .bg(theme.bg_hover)
                                    .px_2()
                                    .rounded_md()
                                    .text_color(theme.text_primary)
                                    .child("Zrušit")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        this.close_modal(cx);
                                    })),
                            ),
                    )
            }
        };

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop)
                .key_context("ModalForm")
                .track_focus(&self.modal_focus_handle)
                .on_action(cx.listener(Self::on_modal_confirm))
                .on_action(cx.listener(Self::on_modal_focus_next))
                .on_action(cx.listener(Self::on_modal_focus_prev))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_discard_confirm_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        self.discard_confirm.as_ref()?;
        let n = self.change_count();
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop)
                // UX-polish §1.4: same shared focus target as
                // `render_modal_overlay` — holds keyboard focus. NO key
                // context here or ever: Enter must stay structurally inert
                // on discard-confirm, admin or app (§3-novela / Global
                // Constraints).
                .track_focus(&self.modal_focus_handle)
                .occlude()
                .child(
                    div()
                        .id("admin-discard-confirm-panel")
                        .w(px(360.))
                        .bg(theme.bg_panel)
                        .border_1()
                        .border_color(theme.border)
                        .rounded_md()
                        .flex()
                        .flex_col()
                        .p_2()
                        .gap_2()
                        .text_color(theme.text_primary)
                        .child(format!("Zahodit neuložené změny? ({n})"))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("admin-discard-confirm-yes")
                                        .cursor_pointer()
                                        .bg(theme.bg_hover)
                                        .px_2()
                                        .rounded_md()
                                        .text_color(theme.danger)
                                        .child("Zahodit")
                                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                            this.discard_confirm_yes(cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id("admin-discard-confirm-no")
                                        .cursor_pointer()
                                        .bg(theme.bg_hover)
                                        .px_2()
                                        .rounded_md()
                                        .text_color(theme.text_primary)
                                        .child("Zpět")
                                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                            this.discard_confirm_no(cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl EventEmitter<AdminEvent> for AdminPanel {}

impl Focusable for AdminPanel {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AdminPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let mut root = div()
            .id("admin-panel")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_panel)
            .child(div().h(px(28.)).px_2().flex().items_center().text_color(theme.text_primary).child("Správa serveru"))
            .child(self.render_sub_nav(cx));

        if self.loading {
            root = root.child(div().px_2().py_1().text_color(theme.text_muted).child("Načítám…"));
        }
        if let Some(err) = self.error.clone() {
            root = root.child(div().px_2().py_1().text_color(theme.danger).child(format!("error: {err}")));
        }

        let body: AnyElement = match self.sub_view {
            AdminSubView::Roles => self.render_roles_body(cx),
            AdminSubView::Privileges => self.render_privileges_body(cx),
            AdminSubView::Databases => self.render_databases_body(cx),
        };
        root = root.child(body);

        if let Some(bar) = self.render_apply_bar(cx) {
            root = root.child(bar);
        }
        if let Some(overlay) = self.render_modal_overlay(cx) {
            root = root.child(overlay);
        }
        if let Some(overlay) = self.render_discard_confirm_overlay(cx) {
            root = root.child(overlay);
        }

        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_state::Engine;

    // CURATION item 6's REQUIRED UI-level test: entry point hidden for
    // SQLite, disabled for read-only, enabled otherwise.
    #[test]
    fn admin_entry_state_matrix() {
        assert_eq!(admin_entry_state(Some(Engine::Sqlite), false), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(Some(Engine::Sqlite), true), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(None, false), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(Some(Engine::Postgres), true), AdminEntry::Disabled);
        assert_eq!(admin_entry_state(Some(Engine::Postgres), false), AdminEntry::Enabled);
        // G15 T8 ON-flip: MSSQL admin follows the same read_only-gated
        // Disabled/Enabled logic as every other writable engine now — see
        // admin_entry_state's doc comment for the live evidence.
        assert_eq!(admin_entry_state(Some(Engine::Mssql), true), AdminEntry::Disabled);
        assert_eq!(admin_entry_state(Some(Engine::Mssql), false), AdminEntry::Enabled);
        // G16: embedded engine — Hidden regardless of read_only, same
        // posture as Sqlite (explicit arm above the pre-existing Some(_)
        // wildcards, house rule).
        assert_eq!(admin_entry_state(Some(Engine::Duckdb), false), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(Some(Engine::Duckdb), true), AdminEntry::Hidden);
    }

    // UX-polish sweep #9: the M6 password rule mirrored onto admin modals —
    // a modal holding a typed (non-empty) password is NOT closable by Esc,
    // same reasoning as ConnectionDialog in `AppView::on_cancel_query`.
    #[test]
    fn esc_closable_without_password_field() {
        assert!(super::admin_esc_closable(false, true));
    }

    #[test]
    fn esc_closable_with_empty_password() {
        assert!(super::admin_esc_closable(true, true));
    }

    #[test]
    fn esc_not_closable_with_typed_password() {
        assert!(!super::admin_esc_closable(true, false));
    }

    fn rows(cols: &[&str], data: &[&[Option<&str>]]) -> AdminCatalogRows {
        (
            cols.iter().map(|c| c.to_string()).collect(),
            data.iter().map(|r| r.iter().map(|c| c.map(|s| s.to_string())).collect()).collect(),
        )
    }

    #[test]
    fn parse_roles_first_col_is_name_rest_is_detail() {
        let r = rows(
            &["rolname", "rolsuper", "rolcanlogin"],
            &[&[Some("alice"), Some("true"), Some("false")], &[Some("bob"), None, Some("true")]],
        );
        let roles = parse_roles(&r);
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].name, "alice");
        assert_eq!(
            roles[0].detail,
            vec![
                ("rolsuper".to_string(), "true".to_string()),
                ("rolcanlogin".to_string(), "false".to_string()),
            ]
        );
        assert_eq!(roles[1].detail[0], ("rolsuper".to_string(), "—".to_string()));
    }

    #[test]
    fn parse_memberships_reads_role_member_pairs() {
        let r = rows(&["role", "member", "admin_option"], &[&[Some("readers"), Some("bob"), Some("false")]]);
        let m = parse_memberships(&r, false);
        assert_eq!(m, vec![Membership { role: "readers".into(), member: "bob".into(), server_role: false }]);
        let s = parse_memberships(&r, true);
        assert!(s[0].server_role);
    }

    #[test]
    fn membership_toggle_stage_unstage_and_statements() {
        let mut e = MembershipEdits::default();
        // Not a member → first toggle stages an add, second unstages.
        e.toggle("readers", "bob", false, false);
        assert!(e.is_checked("readers", "bob", false, false));
        assert_eq!(e.change_count(), 1);
        e.toggle("readers", "bob", false, false);
        assert!(!e.is_checked("readers", "bob", false, false));
        assert_eq!(e.change_count(), 0);
        // A member → toggle stages a removal.
        e.toggle("writers", "bob", false, true);
        assert!(!e.is_checked("writers", "bob", false, true));
        e.toggle("readers", "bob", false, false);
        assert_eq!(e.change_count(), 2);
        assert!(e.is_dirty());

        let stmts = e.to_statements(Engine::Postgres);
        let sql: Vec<&str> = stmts.iter().map(|s| s.exec_sql.as_str()).collect();
        assert_eq!(sql, vec!["GRANT \"readers\" TO \"bob\"", "REVOKE \"writers\" FROM \"bob\""]);

        let ms = e.to_statements(Engine::Mssql);
        let sql: Vec<&str> = ms.iter().map(|s| s.exec_sql.as_str()).collect();
        assert_eq!(sql, vec!["ALTER ROLE [readers] ADD MEMBER [bob]", "ALTER ROLE [writers] DROP MEMBER [bob]"]);

        e.clear();
        assert!(!e.is_dirty());
    }

    // Review finding (MAJOR): the tab-strip "✕" close guard
    // (`main.rs::AppView::grid_dirty_change_count`) had no `TabContent::
    // Admin` arm at all, so closing a dirty admin tab silently discarded
    // staged writes — no confirm prompt, no " •" indicator either. Both
    // sites now read `AdminPanel::change_count`, which is this exact
    // arithmetic; proven here without a GPUI entity/window (this codebase
    // has no "construct an entity in a test" precedent — see this
    // function's own doc comment).
    #[test]
    fn admin_dirty_count_is_zero_when_clean_and_reflects_role_actions_and_membership_edits() {
        let mut edits = MembershipEdits::default();
        // Zero staged anything -> 0, which `main.rs`'s `(n > 0).then_some(n)`
        // turns into `None` (no close-confirm prompt, no dirty dot).
        assert_eq!(combined_change_count(0, &edits, 0), 0);

        // A membership toggle alone must already dirty the tab (a create-
        // role/drop-role/change-password action isn't the only way to
        // stage a write here).
        edits.toggle("readers", "bob", false, false);
        assert_eq!(combined_change_count(0, &edits, 0), 1);

        // Staged role actions (e.g. one "Nová role…" + one "Smazat roli")
        // add on top of whatever membership edits are staged.
        assert_eq!(combined_change_count(2, &edits, 0), 3);

        edits.clear();
        assert_eq!(combined_change_count(2, &edits, 0), 2, "role actions alone still count once membership is clean");

        // T5: MatrixState::change_count folds into the SAME formula — not a
        // second dirtiness definition.
        assert_eq!(combined_change_count(0, &MembershipEdits::default(), 5), 5);
        assert_eq!(combined_change_count(2, &edits, 5), 7);
    }
}

#[cfg(test)]
mod matrix_tests {
    use super::*;
    use crate::admin_sql::CellState;
    use dbc_state::Engine;

    fn pg_catalog(grantee: &str) -> Vec<(&'static str, AdminCatalogRows)> {
        let object_acl = (
            vec![
                "schema".into(),
                "object".into(),
                "kind".into(),
                "grantee".into(),
                "privilege_type".into(),
                "is_grantable".into(),
            ],
            vec![
                vec![
                    Some("public".into()),
                    Some("users".into()),
                    Some("table".into()),
                    Some(grantee.into()),
                    Some("SELECT".into()),
                    Some("false".into()),
                ],
                vec![
                    Some("public".into()),
                    Some("orders".into()),
                    Some("table".into()),
                    Some("owner".into()),
                    Some("SELECT".into()),
                    Some("true".into()),
                ],
            ],
        );
        let schema_acl = (
            vec!["schema".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
            vec![vec![Some("public".into()), Some(grantee.into()), Some("USAGE".into()), Some("false".into())]],
        );
        let db_acl = (
            vec!["database".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
            vec![vec![Some("appdb".into()), Some(grantee.into()), Some("CONNECT".into()), Some("false".into())]],
        );
        vec![("object_acl", object_acl), ("schema_acl", schema_acl), ("db_acl", db_acl)]
    }

    #[test]
    fn from_catalog_filters_grantee_and_lists_every_object() {
        let m = MatrixState::from_catalog(Engine::Postgres, "bob", &pg_catalog("bob"));
        // orders has only an owner row, but still appears as a matrix row.
        assert_eq!(m.objects, vec!["orders".to_string(), "users".to_string()]);
        assert_eq!(m.effective("users", "SELECT"), CellState::Granted);
        assert_eq!(m.effective("orders", "SELECT"), CellState::NotSet);
        assert_eq!(m.schema_current.get("USAGE"), Some(&CellState::Granted));
        assert_eq!(m.db_current.get("CONNECT"), Some(&CellState::Granted));
    }

    #[test]
    fn mssql_state_desc_maps_deny() {
        let object_perms = (
            vec![
                "schema_name".into(),
                "object_name".into(),
                "grantee".into(),
                "permission_name".into(),
                "state_desc".into(),
            ],
            vec![
                vec![
                    Some("dbo".into()),
                    Some("users".into()),
                    Some("bob".into()),
                    Some("SELECT".into()),
                    Some("DENY".into()),
                ],
                vec![
                    Some("dbo".into()),
                    Some("users".into()),
                    Some("bob".into()),
                    Some("INSERT".into()),
                    Some("GRANT_WITH_GRANT_OPTION".into()),
                ],
            ],
        );
        let schema_perms = (
            vec!["schema_name".into(), "grantee".into(), "permission_name".into(), "state_desc".into()],
            vec![],
        );
        let m = MatrixState::from_catalog(
            Engine::Mssql,
            "bob",
            &[("object_perms", object_perms), ("schema_perms", schema_perms)],
        );
        assert_eq!(m.effective("users", "SELECT"), CellState::Denied);
        assert_eq!(m.effective("users", "INSERT"), CellState::Granted);
    }

    #[test]
    fn click_cycles_and_reverting_clears_the_stage() {
        let mut m = MatrixState::from_catalog(Engine::Postgres, "bob", &pg_catalog("bob"));
        m.click_cell(Engine::Postgres, "orders", "SELECT"); // NotSet -> Granted
        assert_eq!(m.effective("orders", "SELECT"), CellState::Granted);
        assert_eq!(m.change_count(), 1);
        m.click_cell(Engine::Postgres, "orders", "SELECT"); // Granted -> NotSet == committed
        assert_eq!(m.change_count(), 0);
        assert!(!m.is_dirty());
        // pg bi-state: no click sequence ever reaches Denied.
        for _ in 0..6 {
            m.click_cell(Engine::Postgres, "users", "SELECT");
            assert_ne!(m.effective("users", "SELECT"), CellState::Denied);
        }
    }

    #[test]
    fn to_statements_groups_same_object_same_target() {
        let mut m = MatrixState::from_catalog(Engine::Postgres, "bob", &pg_catalog("bob"));
        m.click_cell(Engine::Postgres, "orders", "SELECT"); // grant
        m.click_cell(Engine::Postgres, "orders", "INSERT"); // grant
        m.click_cell(Engine::Postgres, "users", "SELECT"); // revoke (was granted)
        m.click_schema_cell(Engine::Postgres, "CREATE"); // grant
        m.click_db_cell("TEMP"); // grant
        let stmts = m.to_statements(Engine::Postgres, "public", "bob", "appdb").unwrap();
        let sql: Vec<&str> = stmts.iter().map(|s| s.exec_sql.as_str()).collect();
        assert_eq!(
            sql,
            vec![
                "GRANT SELECT, INSERT ON \"public\".\"orders\" TO \"bob\"",
                "REVOKE SELECT ON \"public\".\"users\" FROM \"bob\"",
                "GRANT CREATE ON SCHEMA \"public\" TO \"bob\"",
                "GRANT TEMP ON DATABASE \"appdb\" TO \"bob\"",
            ]
        );
    }

    #[test]
    fn to_statements_mssql_emits_deny() {
        let m0 = (
            vec![
                "schema_name".into(),
                "object_name".into(),
                "grantee".into(),
                "permission_name".into(),
                "state_desc".into(),
            ],
            vec![vec![
                Some("dbo".into()),
                Some("users".into()),
                Some("bob".into()),
                Some("SELECT".into()),
                Some("GRANT".into()),
            ]],
        );
        let s0 = (vec!["schema_name".into(), "grantee".into(), "permission_name".into(), "state_desc".into()], vec![]);
        let mut m = MatrixState::from_catalog(Engine::Mssql, "bob", &[("object_perms", m0), ("schema_perms", s0)]);
        m.click_cell(Engine::Mssql, "users", "SELECT"); // Granted -> Denied
        let stmts = m.to_statements(Engine::Mssql, "dbo", "bob", "").unwrap();
        assert_eq!(stmts[0].exec_sql, "DENY SELECT ON [dbo].[users] TO [bob]");
    }

    // Not in the plan's own test list, but directly exercises the
    // "errors-are-values backstop" the Grounding calls out: an `Err` from
    // `admin_sql`'s own refusal (here: SQLite has no schema-level
    // privileges — unreachable via the real UI, since the admin panel is
    // `Hidden` for SQLite entirely) must bubble out of `to_statements` as
    // `Err`, never panic — `AdminPanel::request_apply` depends on this to
    // surface it in `self.error` instead of crashing.
    #[test]
    fn to_statements_bubbles_admin_sql_refusals_as_err() {
        let mut m = MatrixState::default();
        m.click_schema_cell(Engine::Sqlite, "USAGE"); // stages NotSet -> Granted
        let err = m.to_statements(Engine::Sqlite, "s", "grantee", "db").unwrap_err();
        assert!(!err.is_empty());
    }

    // `AdminPanel::apply_catalog`'s routing decision (whole-batch into
    // MatrixState vs. per-label into the Roles parsers) — proven against
    // the ACTUAL labels `admin_sql::privileges_catalog`/`roles_catalog`
    // emit for both engines, without constructing an `AdminPanel` entity.
    #[test]
    fn is_privileges_batch_detects_both_engines_pg_and_mssql_label_sets() {
        assert!(is_privileges_batch(&pg_catalog("bob")));
        let mssql = admin_sql::privileges_catalog(Engine::Mssql, "dbo")
            .into_iter()
            .map(|(l, _)| (l, (Vec::new(), Vec::new())))
            .collect::<Vec<_>>();
        assert!(is_privileges_batch(&mssql));
        // The Roles sub-view's own labels must NEVER be mistaken for a
        // privileges batch (or vice versa) — that's the whole point of
        // apply_catalog routing on this predicate.
        let roles = admin_sql::roles_catalog(Engine::Postgres)
            .into_iter()
            .map(|(l, _)| (l, (Vec::new(), Vec::new())))
            .collect::<Vec<_>>();
        assert!(!is_privileges_batch(&roles));
    }
}

#[cfg(test)]
mod sizes_tests {
    use super::*;
    use dbc_state::Engine;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn bar_fraction_clamps_and_handles_zero_max() {
        assert_eq!(bar_fraction(0, 0), 0.0);
        assert_eq!(bar_fraction(50, 100), 0.5);
        assert_eq!(bar_fraction(100, 100), 1.0);
    }

    fn rows(cols: &[&str], data: &[&[Option<&str>]]) -> AdminCatalogRows {
        (
            cols.iter().map(|c| c.to_string()).collect(),
            data.iter().map(|r| r.iter().map(|c| c.map(|s| s.to_string())).collect()).collect(),
        )
    }

    #[test]
    fn parse_db_sizes_pg_has_bytes_mssql_has_none() {
        let pg = rows(&["datname", "bytes"], &[&[Some("appdb"), Some("1048576")]]);
        assert_eq!(parse_db_sizes(Engine::Postgres, &pg), vec![("appdb".to_string(), Some(1_048_576))]);
        let ms = rows(
            &["name", "database_id", "create_date", "state_desc"],
            &[&[Some("appdb"), Some("5"), Some("2026-01-01"), Some("ONLINE")]],
        );
        assert_eq!(parse_db_sizes(Engine::Mssql, &ms), vec![("appdb".to_string(), None)]);
    }

    #[test]
    fn parse_schema_sizes_pg_bytes_mssql_kb() {
        let pg = rows(&["schema", "bytes"], &[&[Some("public"), Some("2048")], &[Some("empty"), None]]);
        // NULL SUM (schema with no tables) → 0, not a crash.
        assert_eq!(
            parse_schema_sizes(Engine::Postgres, &pg),
            vec![("public".to_string(), 2048), ("empty".to_string(), 0)]
        );
        let ms = rows(&["schema_name", "reserved_kb", "used_kb"], &[&[Some("dbo"), Some("16"), Some("8")]]);
        assert_eq!(parse_schema_sizes(Engine::Mssql, &ms), vec![("dbo".to_string(), 16 * 1024)]);
    }

    // Not in the plan's own test list, but exercises the headline value
    // half the Grounding calls for: pg uses pg_size_pretty's column
    // verbatim; MSSQL has no such column and must be derived via
    // format_bytes from data_mb+log_mb.
    #[test]
    fn current_db_size_label_pg_uses_pretty_column_mssql_derives_from_mb() {
        let pg = rows(&["bytes", "pretty"], &[&[Some("1048576"), Some("1024 kB")]]);
        assert_eq!(current_db_size_label(Engine::Postgres, &pg), Some("1024 kB".to_string()));

        let ms = rows(
            &["database_name", "data_mb", "log_mb"],
            &[&[Some("appdb"), Some("8.00"), Some("2.00")]],
        );
        // (8 + 2) MB = 10 MB, formatted through format_bytes.
        assert_eq!(current_db_size_label(Engine::Mssql, &ms), Some(format_bytes(10 * 1024 * 1024)));

        // No rows at all (still loading) -> None, not a panic.
        let empty = rows(&["bytes", "pretty"], &[]);
        assert_eq!(current_db_size_label(Engine::Postgres, &empty), None);
    }

    // `AdminPanel::apply_catalog`'s T6 routing predicate, proven against
    // the ACTUAL labels `admin_sql::sizes_catalog` emits for both engines
    // — same discipline as T5's `is_privileges_batch` test.
    #[test]
    fn is_sizes_batch_detects_both_engines_and_never_privileges_or_roles_labels() {
        let pg = admin_sql::sizes_catalog(Engine::Postgres)
            .into_iter()
            .map(|(l, _)| (l, (Vec::new(), Vec::new())))
            .collect::<Vec<_>>();
        assert!(is_sizes_batch(&pg));
        let mssql = admin_sql::sizes_catalog(Engine::Mssql)
            .into_iter()
            .map(|(l, _)| (l, (Vec::new(), Vec::new())))
            .collect::<Vec<_>>();
        assert!(is_sizes_batch(&mssql));

        let privileges = admin_sql::privileges_catalog(Engine::Postgres, "public")
            .into_iter()
            .map(|(l, _)| (l, (Vec::new(), Vec::new())))
            .collect::<Vec<_>>();
        assert!(!is_sizes_batch(&privileges));
        let roles = admin_sql::roles_catalog(Engine::Postgres)
            .into_iter()
            .map(|(l, _)| (l, (Vec::new(), Vec::new())))
            .collect::<Vec<_>>();
        assert!(!is_sizes_batch(&roles));
    }
}
