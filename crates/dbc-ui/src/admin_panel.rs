//! G10 T4: "Správa serveru" — the admin panel shell, singleton per
//! connection, plus its first sub-view ("Role a členství"). T5/T6 extend
//! this same file with the "Oprávnění" (privileges matrix) and "Databáze a
//! schémata" sub-views.
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
//!   4. `AdminModal`/`AdminPanel`/`AdminEvent` — the GPUI entity: owns the
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

use std::collections::BTreeSet;

use dbc_state::Engine;
use gpui::{
    div, prelude::*, px, rgb, rgba, AnyElement, ClickEvent, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Window,
};

use crate::admin_sql::{self, WriteStatement};
use crate::connections_ui;
use crate::runner::AdminCatalogRows;

/// Tab-strip singleton dedup key (`tabs.rs::ResultTab::preview_key`) —
/// there is only ever one admin tab per connection at a time (design §2:
/// "one tab, per connection, singleton").
pub const ADMIN_PREVIEW_KEY: &str = "__admin__";

/// Grid's diff-tint yellow (`grid.rs`'s sandbox-edit convention, reused
/// here for staged admin rows/cells — same convention `compare.rs`'s
/// `TINT_CHANGED` documents borrowing independently).
const STAGED_TINT: u32 = 0xf9e2af;

// ---------------------------------------------------------------------
// 1. Entry-point gate (design §2, CURATION item 6's UI-level half).
// ---------------------------------------------------------------------

/// The entry-point gate (design §2), pure and unit-tested — the UI-level
/// half of CURATION item 6 (the runner's `guard_not_read_only` is the
/// OTHER half, unchanged, still the sole write choke point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminEntry {
    /// SQLite (feature-exempt, design §0) or no active connection at all —
    /// the tree row and palette action are both absent entirely.
    Hidden,
    /// A real (pg/MSSQL) read-only connection — the tree row renders
    /// greyed with a "pouze pro čtení" hint; the palette has no
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
/// own count. Extracted so the tab-strip "✕" close guard's dirtiness
/// contract (`main.rs::AppView::grid_dirty_change_count`'s `Admin` arm —
/// review finding: that match had NO `Admin` arm at all, so closing a
/// dirty admin tab silently discarded staged writes) is directly
/// unit-testable without constructing a GPUI `AdminPanel` entity.
fn combined_change_count(staged_role_actions: usize, membership_edits: &MembershipEdits) -> usize {
    staged_role_actions + membership_edits.change_count()
}

// ---------------------------------------------------------------------
// 4. GPUI entity.
// ---------------------------------------------------------------------

/// Which boolean flag on `AdminModal::NewRole` a checkbox click toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleFlagKind {
    Login,
    Superuser,
    Createdb,
    Createrole,
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
    // T6 adds NewSchema/DropSchema here.
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

    modal: Option<AdminModal>,
    /// Sub-view switch requested while the CURRENT sub-view is dirty —
    /// renders the "Zahodit neuložené změny?" prompt instead of switching
    /// silently (design §2).
    discard_confirm: Option<AdminSubView>,

    #[allow(dead_code)] // reserved for future keyboard/escape handling, same posture as SchemaTree's.
    focus_handle: FocusHandle,
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
            modal: None,
            discard_confirm: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn conn_identity(&self) -> &str {
        &self.conn_identity
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
    pub fn apply_catalog(&mut self, rows: Vec<(&'static str, AdminCatalogRows)>, cx: &mut Context<Self>) {
        self.loading = false;
        self.error = None;
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

    /// Post-Apply-success: clear staged sets + re-request the active
    /// sub-view's catalog (design §2/§3's "one transaction per user-visible
    /// action, then refresh").
    pub fn on_apply_success(&mut self, cx: &mut Context<Self>) {
        self.staged_role_actions.clear();
        self.membership_edits.clear();
        self.loading = true;
        cx.notify();
        if let Some(queries) = self.fetch_queries_for(self.sub_view) {
            cx.emit(AdminEvent::FetchCatalog { queries });
        }
    }

    fn fetch_queries_for(&self, sub_view: AdminSubView) -> Option<Vec<(&'static str, String)>> {
        match sub_view {
            AdminSubView::Roles => Some(admin_sql::roles_catalog(self.engine)),
            // T5/T6 wire these to privileges_catalog/sizes_catalog.
            AdminSubView::Privileges | AdminSubView::Databases => None,
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
    /// too).
    pub fn change_count(&self) -> usize {
        combined_change_count(self.staged_role_actions.len(), &self.membership_edits)
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
            self.discard_confirm = Some(target);
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
        self.loading = true;
        cx.notify();
        if let Some(queries) = self.fetch_queries_for(target) {
            cx.emit(AdminEvent::FetchCatalog { queries });
        }
    }

    fn discard_confirm_yes(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.discard_confirm.take() else { return };
        self.switch_sub_view(target, cx);
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
        let name = cx.new(|cx| connections_ui::TextField::new(cx, "název role", false));
        let password = cx.new(|cx| connections_ui::TextField::new(cx, "heslo", true));
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
        let password = cx.new(|cx| connections_ui::TextField::new(cx, "nové heslo", true));
        let focus = password.focus_handle(cx);
        self.modal = Some(AdminModal::ChangePassword { role, password });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        cx.notify();
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

    fn discard_staged(&mut self, cx: &mut Context<Self>) {
        self.staged_role_actions.clear();
        self.membership_edits.clear();
        cx.notify();
    }

    fn request_apply(&mut self, cx: &mut Context<Self>) {
        let mut statements = self.staged_role_actions.clone();
        statements.extend(self.membership_edits.to_statements(self.engine));
        if statements.is_empty() {
            return;
        }
        cx.emit(AdminEvent::RequestApply { statements, warning: None });
    }

    // -------------------------------------------------------------
    // Render helpers.
    // -------------------------------------------------------------

    fn render_sub_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = [
            (AdminSubView::Roles, "Role a členství"),
            (AdminSubView::Privileges, "Oprávnění"),
            (AdminSubView::Databases, "Databáze a schémata"),
        ];
        let mut row = div().flex().flex_row().gap_2().px_2().py_1().bg(rgb(0x181825));
        for (view, label) in tabs {
            let active = view == self.sub_view;
            row = row.child(
                div()
                    .id(format!("admin-subnav-{label}"))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(active, |d| d.bg(rgb(0x313244)))
                    .text_color(if active { rgb(0xcdd6f4) } else { rgb(0xa6adc8) })
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.request_sub_view(view, cx);
                    })),
            );
        }
        row
    }

    fn render_roles_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let roles = self.roles.clone();
        let selected = self.selected_role.clone();

        let mut list = div().flex().flex_col().w(px(220.)).overflow_hidden().border_r_1().border_color(rgb(0x313244));
        for r in &roles {
            let is_selected = selected.as_deref() == Some(r.name.as_str());
            let name_for_click = r.name.clone();
            list = list.child(
                div()
                    .id(format!("admin-role-row-{}", r.name))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .when(is_selected, |d| d.bg(rgb(0x45475a)))
                    .hover(|s| s.bg(rgb(0x313244)))
                    .text_color(rgb(0xcdd6f4))
                    .child(r.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.select_role(name_for_click.clone(), cx);
                    })),
            );
        }

        let mut detail = div().flex().flex_col().flex_1().p_2().gap_2().text_color(rgb(0xcdd6f4));
        if let Some(sel) = &selected {
            if let Some(row) = roles.iter().find(|r| &r.name == sel) {
                for (k, v) in &row.detail {
                    detail = detail.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(div().w(px(160.)).text_color(rgb(0xa6adc8)).child(k.clone()))
                            .child(div().child(v.clone())),
                    );
                }
            }

            // "Členem v" — one checkbox per known role name, plus (MSSQL)
            // a second heading for server-scoped roles.
            detail = detail.child(div().mt_2().text_color(rgb(0xa6adc8)).child("Členem v"));
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
                    detail = detail.child(div().mt_2().text_color(rgb(0xa6adc8)).child("Členem v (server)"));
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
            detail = detail.child(div().text_color(rgb(0x6c7086)).child("Vyberte roli vlevo."));
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
                    .bg(rgb(0x313244))
                    .px_2()
                    .rounded_md()
                    .text_color(rgb(0xcdd6f4))
                    .child("Nová role…")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_new_role_modal(window, cx);
                    })),
            )
            .child(
                div()
                    .id("admin-drop-role")
                    .when(selected.is_some(), |d| d.cursor_pointer())
                    .bg(rgb(0x313244))
                    .px_2()
                    .rounded_md()
                    .text_color(if selected.is_some() { rgb(0xf38ba8) } else { rgb(0x6c7086) })
                    .child("Smazat roli")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.stage_drop_role(cx);
                    })),
            )
            .child(
                div()
                    .id("admin-change-password")
                    .when(selected.is_some(), |d| d.cursor_pointer())
                    .bg(rgb(0x313244))
                    .px_2()
                    .rounded_md()
                    .text_color(if selected.is_some() { rgb(0xcdd6f4) } else { rgb(0x6c7086) })
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
            .when(staged, |d| d.bg(rgba((STAGED_TINT << 8) | 0x40)))
            .text_color(rgb(0xcdd6f4))
            .child(mark)
            .child(role)
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                let currently = this.is_member(&role_for_click, &member_for_click, server_role);
                this.membership_edits.toggle(&role_for_click, &member_for_click, server_role, currently);
                cx.notify();
            }))
    }

    fn render_apply_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                .bg(rgb(0x181825))
                .text_color(rgb(0xf9e2af))
                .child(format!("{n} změn"))
                .child(
                    div()
                        .id("admin-apply")
                        .cursor_pointer()
                        .bg(rgb(0x313244))
                        .px_2()
                        .rounded_md()
                        .text_color(rgb(0xa6e3a1))
                        .child("Aplikovat")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.request_apply(cx))),
                )
                .child(
                    div()
                        .id("admin-discard")
                        .cursor_pointer()
                        .bg(rgb(0x313244))
                        .px_2()
                        .rounded_md()
                        .text_color(rgb(0xcdd6f4))
                        .child("Zahodit")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.discard_staged(cx))),
                )
                .into_any_element(),
        )
    }

    fn render_modal_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                            .text_color(rgb(0xcdd6f4))
                            .child(format!("{mark} {label}"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.toggle_new_role_flag(flag, cx);
                            })),
                    );
                }
                div()
                    .id("admin-modal-new-role")
                    .w(px(360.))
                    .bg(rgb(0x1e1e2e))
                    .border_1()
                    .border_color(rgb(0x45475a))
                    .rounded_md()
                    .flex()
                    .flex_col()
                    .p_2()
                    .gap_2()
                    .text_color(rgb(0xcdd6f4))
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
                                    .bg(rgb(0x313244))
                                    .px_2()
                                    .rounded_md()
                                    .text_color(rgb(0xa6e3a1))
                                    .child("Vytvořit")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        this.confirm_new_role(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("admin-modal-cancel")
                                    .cursor_pointer()
                                    .bg(rgb(0x313244))
                                    .px_2()
                                    .rounded_md()
                                    .text_color(rgb(0xcdd6f4))
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
                .bg(rgb(0x1e1e2e))
                .border_1()
                .border_color(rgb(0x45475a))
                .rounded_md()
                .flex()
                .flex_col()
                .p_2()
                .gap_2()
                .text_color(rgb(0xcdd6f4))
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
                                .bg(rgb(0x313244))
                                .px_2()
                                .rounded_md()
                                .text_color(rgb(0xa6e3a1))
                                .child("Změnit")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.confirm_change_password(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("admin-modal-cancel")
                                .cursor_pointer()
                                .bg(rgb(0x313244))
                                .px_2()
                                .rounded_md()
                                .text_color(rgb(0xcdd6f4))
                                .child("Zrušit")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.close_modal(cx);
                                })),
                        ),
                ),
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
                .bg(rgba(0x00000099))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_discard_confirm_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.discard_confirm?;
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
                .bg(rgba(0x00000099))
                .occlude()
                .child(
                    div()
                        .id("admin-discard-confirm-panel")
                        .w(px(360.))
                        .bg(rgb(0x1e1e2e))
                        .border_1()
                        .border_color(rgb(0x45475a))
                        .rounded_md()
                        .flex()
                        .flex_col()
                        .p_2()
                        .gap_2()
                        .text_color(rgb(0xcdd6f4))
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
                                        .bg(rgb(0x313244))
                                        .px_2()
                                        .rounded_md()
                                        .text_color(rgb(0xf38ba8))
                                        .child("Zahodit")
                                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                            this.discard_confirm_yes(cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id("admin-discard-confirm-no")
                                        .cursor_pointer()
                                        .bg(rgb(0x313244))
                                        .px_2()
                                        .rounded_md()
                                        .text_color(rgb(0xcdd6f4))
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
        let mut root = div()
            .id("admin-panel")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .child(div().h(px(28.)).px_2().flex().items_center().text_color(rgb(0xcdd6f4)).child("Správa serveru"))
            .child(self.render_sub_nav(cx));

        if self.loading {
            root = root.child(div().px_2().py_1().text_color(rgb(0xa6adc8)).child("Načítám…"));
        }
        if let Some(err) = self.error.clone() {
            root = root.child(div().px_2().py_1().text_color(rgb(0xf38ba8)).child(format!("error: {err}")));
        }

        let body: AnyElement = match self.sub_view {
            AdminSubView::Roles => self.render_roles_body(cx),
            AdminSubView::Privileges | AdminSubView::Databases => div()
                .flex_1()
                .p_2()
                .text_color(rgb(0x6c7086))
                .child("Bude doplněno.")
                .into_any_element(),
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
        assert_eq!(admin_entry_state(Some(Engine::Mssql), true), AdminEntry::Disabled);
        assert_eq!(admin_entry_state(Some(Engine::Postgres), false), AdminEntry::Enabled);
        assert_eq!(admin_entry_state(Some(Engine::Mssql), false), AdminEntry::Enabled);
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
        assert_eq!(combined_change_count(0, &edits), 0);

        // A membership toggle alone must already dirty the tab (a create-
        // role/drop-role/change-password action isn't the only way to
        // stage a write here).
        edits.toggle("readers", "bob", false, false);
        assert_eq!(combined_change_count(0, &edits), 1);

        // Staged role actions (e.g. one "Nová role…" + one "Smazat roli")
        // add on top of whatever membership edits are staged.
        assert_eq!(combined_change_count(2, &edits), 3);

        edits.clear();
        assert_eq!(combined_change_count(2, &edits), 2, "role actions alone still count once membership is clean");
    }
}
