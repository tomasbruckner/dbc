//! Right-click menus for the sidebar tree — the pure half.
//!
//! `menu_for` decides WHAT a menu contains; `schema_tree.rs` draws it and
//! `main.rs` performs the emitted [`TreeEvent`]. Keeping the decision here,
//! as a function from a row to a list, is what makes the menus testable at
//! all: the tree renders through GPUI, which has no headless harness in this
//! repo, so anything decided inside `render` can only ever be checked by
//! hand.
//!
//! **Ordering rule.** Items are ordered by how often they are used, not
//! grouped by category, and the same item keeps the same position across row
//! types — „Obnovit" is always last, „Kopírovat…" always in the middle
//! group. That is what lets a menu be used without reading it.
//!
//! **Destructive items** (`DROP`, `TRUNCATE`) are last, after a separator,
//! marked [`MenuItem::danger`], and are omitted entirely on a read-only
//! connection. They do NOT execute on click: they emit an event that opens
//! the existing Apply confirm dialog showing the exact SQL, which is this
//! codebase's rule for every write path.
//!
//! Dropping a DATABASE is deliberately absent. „Správa serveru" already owns
//! that, with a CASCADE warning; a second entry point to the single most
//! destructive operation in the app is not discoverability, it is a
//! liability.

use dbc_core::{Dialect, SchemaSnapshot, TableKind};
use dbc_state::FavouriteObject;

use crate::schema_tree::{DropKind, GenKind, NodeId, SidebarRow, TreeEvent};

#[derive(Debug, Clone, PartialEq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub event: TreeEvent,
    /// Rendered red and always placed after a separator. Purely visual —
    /// the actual protection is the confirm dialog the event opens.
    pub danger: bool,
}

fn item(label: impl Into<String>, event: TreeEvent) -> MenuEntry {
    MenuEntry::Item(MenuItem { label: label.into(), event, danger: false })
}

fn danger(label: impl Into<String>, event: TreeEvent) -> MenuEntry {
    MenuEntry::Item(MenuItem { label: label.into(), event, danger: true })
}

/// Everything `menu_for` needs that is not in the row itself.
pub struct MenuCtx<'a> {
    /// The active connection's read-only flag. Suppresses every write item.
    pub read_only: bool,
    /// `None` = no active connection; SQL-generating items are omitted
    /// rather than guessed, since quoting is dialect-specific.
    pub dialect: Option<Dialect>,
    /// The active database's schema snapshot, for column lists.
    pub snapshot: Option<&'a SchemaSnapshot>,
    /// Already-starred objects, so the ★ item can say „Odebrat".
    pub favourites: &'a [FavouriteObject],
    /// The ACTIVE scope. A favourite is keyed by connection and database as
    /// well as name, so a menu built without them would star the object in
    /// the wrong place — and would then fail to find it again to unstar.
    pub conn_id: &'a str,
    pub database: Option<String>,
}

impl MenuCtx<'_> {
    fn table<'s>(&'s self, schema: &str, name: &str) -> Option<&'s dbc_core::TableInfo> {
        self.snapshot?.tables.iter().find(|t| {
            t.name.eq_ignore_ascii_case(name)
                && t.schema.as_deref().unwrap_or("").eq_ignore_ascii_case(schema)
        })
    }

    fn routine_ddl(&self, schema: &str, name: &str) -> Option<String> {
        self.snapshot?
            .routines
            .iter()
            .find(|r| {
                r.name.eq_ignore_ascii_case(name)
                    && r.schema.as_deref().unwrap_or("").eq_ignore_ascii_case(schema)
            })
            .and_then(|r| r.ddl.clone())
    }

    /// The single schema an ER diagram opened from a DATABASE row would
    /// mean, or `None` when the question has no one answer.
    ///
    /// `None` in three distinct cases, all of which must omit the item
    /// rather than guess:
    ///   * the row is not the ACTIVE database — the snapshot then describes
    ///     a different database and the diagram would be of the wrong one;
    ///   * the snapshot has no tables — an empty diagram is not an answer;
    ///   * the snapshot spans several schemas — the diagram is per-schema by
    ///     design (§3 CURATION), so the schema rows own that case.
    ///
    /// The `Some(None)` arm is a real schema-less engine (SQLite), matching
    /// `AppView::resolve_er_diagram_schema`, which does this for the
    /// palette's zero-argument entry point.
    fn er_diagram_schema(&self, db: &str) -> Option<Option<String>> {
        if self.database.as_deref() != Some(db) {
            return None;
        }
        let snapshot = self.snapshot?;
        let mut schemas: Vec<Option<String>> =
            snapshot.tables.iter().map(|t| t.schema.clone()).collect();
        schemas.sort();
        schemas.dedup();
        if schemas.len() == 1 { schemas.into_iter().next() } else { None }
    }

    fn is_favourite(&self, kind: &str, schema: &str, name: &str) -> bool {
        self.favourites.iter().any(|f| {
            f.kind == kind
                && f.connection_id == self.conn_id
                && f.database == self.database
                && f.name.eq_ignore_ascii_case(name)
                && f.schema.as_deref().unwrap_or("").eq_ignore_ascii_case(schema)
        })
    }
}

/// `Some(schema)` for a real schema, `None` for the schema-less engines —
/// the `NodeId` convention stores `""` for „no schema".
fn schema_opt(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// The menu for one row. Empty = no menu (right-click does nothing), which
/// is the honest answer for a „Načítám…" notice.
pub fn menu_for(row: &SidebarRow, ctx: &MenuCtx) -> Vec<MenuEntry> {
    match row {
        SidebarRow::Connection { conn_id } => connection_menu(conn_id, ctx),
        SidebarRow::Database { conn_id, db } => database_menu(conn_id, db, ctx),
        SidebarRow::Inner { node, .. } | SidebarRow::Pinned(node) => inner_menu(node, ctx),
        SidebarRow::ScriptsRoot => vec![
            item("Nový skript…", TreeEvent::ScriptCreate { parent_rel: String::new() }),
            MenuEntry::Separator,
            item("Obnovit", TreeEvent::ScriptsRefresh),
        ],
        SidebarRow::ScriptFolder { rel } => vec![
            item("Nový skript zde…", TreeEvent::ScriptCreate { parent_rel: rel.clone() }),
            MenuEntry::Separator,
            item("Přejmenovat…", TreeEvent::ScriptRename { rel: rel.clone(), is_dir: true }),
            item("Kopírovat cestu", TreeEvent::CopyText { what: "cesta".into(), text: rel.clone() }),
            MenuEntry::Separator,
            danger("Smazat…", TreeEvent::ScriptDelete { rel: rel.clone(), is_dir: true }),
        ],
        SidebarRow::ScriptFile { rel } => vec![
            item("Otevřít v editoru", TreeEvent::ScriptOpen { rel: rel.clone() }),
            item("Spustit…", TreeEvent::ScriptRunFile { rel: rel.clone() }),
            MenuEntry::Separator,
            item("Přejmenovat…", TreeEvent::ScriptRename { rel: rel.clone(), is_dir: false }),
            item("Kopírovat cestu", TreeEvent::CopyText { what: "cesta".into(), text: rel.clone() }),
            MenuEntry::Separator,
            danger("Smazat…", TreeEvent::ScriptDelete { rel: rel.clone(), is_dir: false }),
        ],
        SidebarRow::Folder { path } => folder_menu(path),
        // A notice row is a status message, not an object.
        SidebarRow::Notice { .. } | SidebarRow::ScriptNotice { .. } => Vec::new(),
    }
}

/// A folder holds saved connections, which live in `config.toml` — nothing
/// here touches a database, so nothing here is gated on `read_only`, which
/// is about the SERVER.
///
/// „Smazat" is marked `danger` for its weight, not its blast radius:
/// deleting a folder moves its connections to the parent and destroys
/// none of them (see `folders::delete`). The confirm says so, because
/// „Smazat složku" on a folder full of connections reads like a threat.
fn folder_menu(path: &[String]) -> Vec<MenuEntry> {
    let p = path.to_vec();
    vec![
        // FIRST, above the folder operations: a folder exists to hold
        // connections, so „make one here" is what a right click on it is
        // most often for. The folder is carried, so the dialog opens with
        // this path already in its „Slozka" field.
        item("Nové připojení zde…", TreeEvent::ConnectionCreate { folder: p.clone() }),
        item("Nová podsložka…", TreeEvent::FolderCreate { parent: p.clone() }),
        MenuEntry::Separator,
        item("Přejmenovat…", TreeEvent::FolderRename { path: p.clone() }),
        item(
            "Kopírovat cestu",
            TreeEvent::CopyText { what: "cesta".into(), text: p.join("/") },
        ),
        MenuEntry::Separator,
        danger("Smazat složku…", TreeEvent::FolderDelete { path: p }),
    ]
}

fn connection_menu(conn_id: &str, ctx: &MenuCtx) -> Vec<MenuEntry> {
    let id = conn_id.to_string();
    let mut out = vec![
        item("Nastavit jako aktivní", TreeEvent::SwitchToDatabase { conn_id: id.clone(), db: None }),
        MenuEntry::Separator,
        item("Správa serveru…", TreeEvent::OpenAdmin),
        item("Monitor…", TreeEvent::OpenMonitorFor { conn_id: id.clone() }),
        item("Porovnat schémata…", TreeEvent::OpenCompareFor { conn_id: id.clone() }),
        MenuEntry::Separator,
        item("Záloha databáze…", TreeEvent::BackupFor { conn_id: id.clone(), db: None }),
    ];
    // Restore WRITES. It is the one item here that can destroy an existing
    // database, so it follows the same read-only rule as DROP/TRUNCATE
    // rather than the „offer it and refuse at click time" posture the
    // palette uses (that posture exists because a palette has no disabled
    // row; a context menu simply omits).
    if !ctx.read_only {
        out.push(item("Obnovit ze zálohy…", TreeEvent::RestoreFor { conn_id: id.clone(), db: None }));
    }
    out.extend([
        MenuEntry::Separator,
        item("Upravit připojení…", TreeEvent::EditConnection { conn_id: id.clone() }),
        item("Kopírovat jméno", TreeEvent::CopyText { what: "jméno".into(), text: id.clone() }),
        MenuEntry::Separator,
        item("Obnovit seznam databází", TreeEvent::LoadDatabases { conn_id: id.clone() }),
        // Last and after a separator, like every other destructive item —
        // and NOT gated on `read_only`, which is about the SERVER. This
        // deletes an entry from this app's own list (user report,
        // 2026-08-31: „nejde smazat connection při pravým kliku"); the
        // database it points at is not touched.
        MenuEntry::Separator,
        danger("Smazat připojení…", TreeEvent::ConnectionDelete { conn_id: id }),
    ]);
    out
}

fn database_menu(conn_id: &str, db: &str, ctx: &MenuCtx) -> Vec<MenuEntry> {
    let (id, dbn) = (conn_id.to_string(), db.to_string());
    let mut out = vec![
        item(
            "Nastavit jako aktivní",
            TreeEvent::SwitchToDatabase { conn_id: id.clone(), db: Some(dbn.clone()) },
        ),
        MenuEntry::Separator,
    ];
    // Was `schema: None` hardcoded, which on any engine WITH schemas
    // filtered the snapshot down to zero tables and opened a blank diagram
    // (user report, 2026-08-29: „nic se neděje"). A database row can only
    // name a schema when there is exactly one to name.
    if let Some(schema) = ctx.er_diagram_schema(db) {
        out.push(item("ER diagram…", TreeEvent::OpenErDiagram { schema }));
    }
    out.push(item(
        "Záloha…",
        TreeEvent::BackupFor { conn_id: id.clone(), db: Some(dbn.clone()) },
    ));
    if !ctx.read_only {
        out.push(item(
            "Obnovit ze zálohy…",
            TreeEvent::RestoreFor { conn_id: id.clone(), db: Some(dbn.clone()) },
        ));
    }
    out.extend([
        MenuEntry::Separator,
        item("Kopírovat jméno", TreeEvent::CopyText { what: "jméno".into(), text: dbn.clone() }),
        MenuEntry::Separator,
        item("Obnovit schéma", TreeEvent::LoadSchema { conn_id: id, db: dbn }),
    ]);
    out
}

fn inner_menu(node: &NodeId, ctx: &MenuCtx) -> Vec<MenuEntry> {
    match node {
        NodeId::Schema(s) => vec![
            item("ER diagram…", TreeEvent::OpenErDiagram { schema: schema_opt(s) }),
            MenuEntry::Separator,
            item("Kopírovat jméno", TreeEvent::CopyText { what: "jméno".into(), text: s.clone() }),
            MenuEntry::Separator,
            item("Obnovit", TreeEvent::RefreshRequested),
        ],
        NodeId::Section(..) => vec![item("Obnovit", TreeEvent::RefreshRequested)],
        NodeId::Table(schema, name) => table_menu(schema, name, ctx),
        NodeId::Column(schema, table, col) => column_menu(schema, table, col, ctx),
        NodeId::Routine(schema, name) => {
            object_menu(schema, name, "routine", DropKind::Routine, ctx)
        }
        NodeId::Trigger(schema, name) => {
            object_menu(schema, name, "trigger", DropKind::Trigger, ctx)
        }
        NodeId::Sequence(schema, name) => {
            object_menu(schema, name, "sequence", DropKind::Sequence, ctx)
        }
        NodeId::Index(schema, _table, name) => {
            object_menu(schema, name, "index", DropKind::Index, ctx)
        }
        NodeId::Favourite(kind, schema, name) => {
            // A favourite row mirrors a real object; give it the same menu
            // rather than a second, thinner one that drifts.
            if kind == "table" || kind == "view" {
                table_menu(schema, name, ctx)
            } else {
                object_menu(schema, name, kind, DropKind::Routine, ctx)
            }
        }
        NodeId::FavouriteSection | NodeId::AdminRoot => Vec::new(),
    }
}

fn table_menu(schema: &str, name: &str, ctx: &MenuCtx) -> Vec<MenuEntry> {
    let s_opt = schema_opt(schema);
    let is_view = ctx
        .table(schema, name)
        .is_some_and(|t| matches!(t.kind, TableKind::View | TableKind::MaterializedView));
    let mut out = vec![
        item(
            "Otevřít data",
            TreeEvent::OpenPreviewHere { schema: s_opt.clone(), table: name.to_string() },
        ),
        item(
            "Počet řádků",
            TreeEvent::CountRows { schema: s_opt.clone(), table: name.to_string() },
        ),
        MenuEntry::Separator,
    ];

    // SQL generation needs the dialect for quoting and the snapshot for the
    // column list. With neither there is nothing honest to generate, so the
    // group is omitted rather than emitting unquoted or column-less SQL.
    if let (Some(dialect), Some(t)) = (ctx.dialect, ctx.table(schema, name)) {
        let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
        for (label, kind) in [
            ("SELECT", GenKind::Select),
            ("INSERT", GenKind::Insert),
            ("UPDATE", GenKind::Update),
        ] {
            // INSERT/UPDATE are templates for WRITES; a read-only
            // connection cannot run them, and offering SQL that will be
            // refused on execution is a worse experience than not offering
            // it.
            if ctx.read_only && kind != GenKind::Select {
                continue;
            }
            out.push(item(
                format!("Generovat {label}"),
                TreeEvent::GenerateSql {
                    kind,
                    sql: generate_sql(kind, dialect, s_opt.as_deref(), name, &cols),
                },
            ));
        }
        out.push(item(
            "Kopírovat seznam sloupců",
            TreeEvent::CopyText { what: "sloupce".into(), text: cols.join(", ") },
        ));
    }
    if let Some(t) = ctx.table(schema, name) {
        // Built here, not in `main.rs`: the DDL comes from the snapshot the
        // tree already holds, and `OpenDdl` is the event the double-click
        // path has always used — one DDL route, not two.
        let ddl = t.ddl.clone().unwrap_or_else(|| dbc_core::synthesize_create_table(t));
        out.push(item("Zobrazit DDL", TreeEvent::OpenDdl { title: name.to_string(), ddl }));
    }
    out.push(item(
        "Kopírovat jméno",
        TreeEvent::CopyText { what: "jméno".into(), text: name.to_string() },
    ));
    if let Some(dialect) = ctx.dialect {
        out.push(item(
            "Kopírovat kvalifikované jméno",
            TreeEvent::CopyText {
                what: "jméno".into(),
                text: dbc_core::quote_qualified_d(dialect, s_opt.as_deref(), name),
            },
        ));
    }

    out.push(MenuEntry::Separator);
    if !ctx.read_only {
        out.push(item(
            "Import z CSV…",
            TreeEvent::ImportCsv { schema: s_opt.clone(), table: name.to_string() },
        ));
    }
    out.push(item(
        "Export do CSV…",
        TreeEvent::ExportCsv { schema: s_opt.clone(), table: name.to_string() },
    ));
    let fav_kind = if is_view { "view" } else { "table" };
    out.push(favourite_item(fav_kind, schema, name, ctx));

    if !ctx.read_only {
        out.push(MenuEntry::Separator);
        if !is_view {
            out.push(danger(
                "TRUNCATE…",
                TreeEvent::TruncateTable { schema: s_opt.clone(), table: name.to_string() },
            ));
        }
        out.push(danger(
            "DROP…",
            TreeEvent::DropObject {
                kind: if is_view { DropKind::View } else { DropKind::Table },
                schema: s_opt,
                name: name.to_string(),
            },
        ));
    }
    out
}

fn column_menu(schema: &str, table: &str, col: &str, ctx: &MenuCtx) -> Vec<MenuEntry> {
    let ty = ctx
        .table(schema, table)
        .and_then(|t| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(col)))
        .map(|c| c.data_type.clone());
    let mut out = vec![item(
        "Kopírovat jméno",
        TreeEvent::CopyText { what: "jméno".into(), text: col.to_string() },
    )];
    if let Some(ty) = ty {
        out.push(item("Kopírovat typ", TreeEvent::CopyText { what: "typ".into(), text: ty }));
    }
    out.extend([
        MenuEntry::Separator,
        item(
            "Vložit do editoru",
            TreeEvent::InsertAtCursor { text: col.to_string() },
        ),
    ]);
    out
}

fn object_menu(
    schema: &str,
    name: &str,
    fav_kind: &str,
    drop_kind: DropKind,
    ctx: &MenuCtx,
) -> Vec<MenuEntry> {
    let s_opt = schema_opt(schema);
    let mut out = Vec::new();
    // Only routines carry a stored definition; a trigger/index/sequence row
    // has nothing to show, so the item is omitted rather than opening an
    // empty tab.
    if let Some(ddl) = ctx.routine_ddl(schema, name) {
        out.push(item("Zobrazit DDL", TreeEvent::OpenDdl { title: name.to_string(), ddl }));
    }
    out.push(item("Kopírovat jméno", TreeEvent::CopyText { what: "jméno".into(), text: name.to_string() }));
    out.push(favourite_item(fav_kind, schema, name, ctx));
    if !ctx.read_only {
        out.push(MenuEntry::Separator);
        out.push(danger(
            "DROP…",
            TreeEvent::DropObject { kind: drop_kind, schema: s_opt, name: name.to_string() },
        ));
    }
    out
}

fn favourite_item(kind: &str, schema: &str, name: &str, ctx: &MenuCtx) -> MenuEntry {
    let starred = ctx.is_favourite(kind, schema, name);
    item(
        if starred { "Odebrat z oblíbených" } else { "Přidat do oblíbených" },
        TreeEvent::ToggleFavourite(FavouriteObject {
            connection_id: ctx.conn_id.to_string(),
            database: ctx.database.clone(),
            kind: kind.to_string(),
            schema: schema_opt(schema),
            name: name.to_string(),
        }),
    )
}

/// SELECT/INSERT/UPDATE skeletons.
///
/// Identifiers go through `quote_qualified_d`/`quote_ident_d`, so a column
/// called `order` or `Column Name` comes out runnable rather than as a
/// syntax error the user has to fix by hand. That is the entire reason
/// these are generated from the snapshot instead of typed.
pub fn generate_sql(
    kind: GenKind,
    dialect: Dialect,
    schema: Option<&str>,
    table: &str,
    columns: &[String],
) -> String {
    let q = |c: &String| dbc_core::quote_ident_d(dialect, c);
    let target = dbc_core::quote_qualified_d(dialect, schema, table);
    let cols: Vec<String> = columns.iter().map(q).collect();
    match kind {
        GenKind::Select => {
            let list = if cols.is_empty() { "*".to_string() } else { cols.join(", ") };
            format!("SELECT {list}\nFROM {target}")
        }
        GenKind::Insert => {
            let list = cols.join(", ");
            let holes = vec!["?"; cols.len()].join(", ");
            format!("INSERT INTO {target} ({list})\nVALUES ({holes})")
        }
        GenKind::Update => {
            let sets =
                cols.iter().map(|c| format!("  {c} = ?")).collect::<Vec<_>>().join(",\n");
            // The WHERE is deliberately present and deliberately false-y:
            // an UPDATE skeleton without one is a whole-table update waiting
            // to be run by accident.
            format!("UPDATE {target}\nSET\n{sets}\nWHERE 1 = 0 -- doplňte podmínku")
        }
    }
}

/// The SQL for a `DROP`, built from the same quoting helpers everything
/// else uses.
///
/// Pure and separate from the menu so the exact statement the confirm
/// dialog will show is unit-testable — for a destructive statement, „what
/// will this run" must be answerable without a database.
pub fn drop_sql(kind: DropKind, dialect: Dialect, schema: Option<&str>, name: &str) -> String {
    let target = dbc_core::quote_qualified_d(dialect, schema, name);
    let keyword = match kind {
        DropKind::Table => "TABLE",
        DropKind::View => "VIEW",
        // The tree does not distinguish FUNCTION from PROCEDURE on the node
        // itself, and dropping the wrong one is a plain error rather than a
        // silent mistake, so this stays the one place it is decided.
        DropKind::Routine => "PROCEDURE",
        DropKind::Trigger => "TRIGGER",
        DropKind::Index => "INDEX",
        DropKind::Sequence => "SEQUENCE",
    };
    format!("DROP {keyword} {target}")
}

/// The SQL for emptying a table.
///
/// SQLite has no `TRUNCATE`, so it gets `DELETE FROM` — which is the
/// documented equivalent there, not a silent downgrade. Getting this wrong
/// would mean a confirm dialog showing a statement that cannot run.
pub fn truncate_sql(dialect: Dialect, schema: Option<&str>, table: &str) -> String {
    let target = dbc_core::quote_qualified_d(dialect, schema, table);
    match dialect {
        Dialect::Sqlite => format!("DELETE FROM {target}"),
        _ => format!("TRUNCATE TABLE {target}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{ColumnInfo, TableInfo};

    fn snap() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![
                TableInfo {
                    schema: Some("dbo".into()),
                    name: "orders".into(),
                    kind: TableKind::Table,
                    columns: vec![
                        ColumnInfo { name: "id".into(), data_type: "int".into(), ..Default::default() },
                        ColumnInfo {
                            name: "order".into(),
                            data_type: "nvarchar".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                TableInfo {
                    schema: Some("dbo".into()),
                    name: "v_orders".into(),
                    kind: TableKind::View,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn ctx<'a>(snapshot: &'a SchemaSnapshot, read_only: bool) -> MenuCtx<'a> {
        MenuCtx {
            read_only,
            dialect: Some(Dialect::Mssql),
            snapshot: Some(snapshot),
            favourites: &[],
            conn_id: "c1",
            database: None,
        }
    }

    fn labels(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i.label.clone()),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    fn table_row() -> SidebarRow {
        SidebarRow::Inner {
            conn_id: "c1".into(),
            db: "d".into(),
            node: NodeId::Table("dbo".into(), "orders".into()),
        }
    }

    /// THE rule for this whole feature: a read-only connection offers no
    /// way to write. If this fails, the menu is handing the user an action
    /// the guard will refuse — or worse, one it will not.
    #[test]
    fn a_read_only_connection_offers_no_destructive_item() {
        let s = snap();
        let ro = menu_for(&table_row(), &ctx(&s, true));
        let ls = labels(&ro);
        for banned in ["DROP…", "TRUNCATE…", "Import z CSV…", "Generovat INSERT", "Generovat UPDATE"]
        {
            assert!(!ls.contains(&banned.to_string()), "read-only menu offered {banned}: {ls:?}");
        }
        // …and still offers the read ones, or it is useless.
        assert!(ls.contains(&"Otevřít data".to_string()));
        assert!(ls.contains(&"Generovat SELECT".to_string()));
        assert!(ls.contains(&"Export do CSV…".to_string()));
    }

    #[test]
    fn a_writable_table_offers_truncate_and_drop_last_and_marked_danger() {
        let s = snap();
        let m = menu_for(&table_row(), &ctx(&s, false));
        let ls = labels(&m);
        assert!(ls.contains(&"TRUNCATE…".to_string()));
        assert!(ls.contains(&"DROP…".to_string()));
        // Danger items are last, after a separator, and are the ONLY
        // danger-marked ones.
        let dangerous: Vec<&MenuItem> = m
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) if i.danger => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(dangerous.len(), 2, "{:?}", dangerous.iter().map(|i| &i.label).collect::<Vec<_>>());
        let last_two = &m[m.len() - 2..];
        assert!(matches!(&last_two[0], MenuEntry::Item(i) if i.danger));
        assert!(matches!(&last_two[1], MenuEntry::Item(i) if i.danger));
    }

    /// A view cannot be truncated, and dropping one is `DROP VIEW`.
    #[test]
    fn a_view_offers_drop_but_not_truncate() {
        let s = snap();
        let row = SidebarRow::Inner {
            conn_id: "c1".into(),
            db: "d".into(),
            node: NodeId::Table("dbo".into(), "v_orders".into()),
        };
        let m = menu_for(&row, &ctx(&s, false));
        assert!(!labels(&m).contains(&"TRUNCATE…".to_string()));
        assert!(m.iter().any(|e| matches!(
            e,
            MenuEntry::Item(MenuItem { event: TreeEvent::DropObject { kind: DropKind::View, .. }, .. })
        )));
    }

    /// Generated SQL must be runnable as-is. `order` is a reserved word and
    /// the whole point of generating instead of typing is that quoting is
    /// handled.
    #[test]
    fn generated_sql_quotes_reserved_identifiers() {
        let sql = generate_sql(
            GenKind::Select,
            Dialect::Mssql,
            Some("dbo"),
            "orders",
            &["id".into(), "order".into()],
        );
        assert!(sql.contains("[order]"), "reserved word left bare: {sql}");
        assert!(sql.contains("[dbo].[orders]"), "{sql}");
    }

    /// An UPDATE skeleton without a WHERE is a whole-table update one
    /// keystroke away from running.
    #[test]
    fn a_generated_update_cannot_be_run_by_accident() {
        let sql =
            generate_sql(GenKind::Update, Dialect::Postgres, None, "t", &["a".into(), "b".into()]);
        assert!(sql.contains("WHERE 1 = 0"), "{sql}");
    }

    #[test]
    fn no_snapshot_means_no_generated_sql_rather_than_guessed_sql() {
        let empty = SchemaSnapshot::default();
        let m = menu_for(&table_row(), &MenuCtx {
            read_only: false,
            dialect: Some(Dialect::Mssql),
            snapshot: Some(&empty),
            favourites: &[],
            conn_id: "c1",
            database: None,
        });
        let ls = labels(&m);
        assert!(!ls.iter().any(|l| l.starts_with("Generovat")), "{ls:?}");
        // The row is still useful for the things that need no snapshot.
        assert!(ls.contains(&"Otevřít data".to_string()));
    }

    #[test]
    fn drop_sql_names_the_right_object_kind_and_quotes_it() {
        assert_eq!(
            drop_sql(DropKind::Table, Dialect::Mssql, Some("dbo"), "order"),
            "DROP TABLE [dbo].[order]"
        );
        assert_eq!(
            drop_sql(DropKind::View, Dialect::Postgres, Some("public"), "v"),
            "DROP VIEW \"public\".\"v\""
        );
        assert_eq!(drop_sql(DropKind::Index, Dialect::Sqlite, None, "ix"), "DROP INDEX \"ix\"");
    }

    /// SQLite has no TRUNCATE. A confirm dialog showing a statement the
    /// engine cannot run is worse than no menu item.
    #[test]
    fn truncate_falls_back_to_delete_on_sqlite_only() {
        assert_eq!(truncate_sql(Dialect::Sqlite, None, "t"), "DELETE FROM \"t\"");
        assert_eq!(truncate_sql(Dialect::Mssql, Some("dbo"), "t"), "TRUNCATE TABLE [dbo].[t]");
        assert_eq!(
            truncate_sql(Dialect::Postgres, Some("public"), "t"),
            "TRUNCATE TABLE \"public\".\"t\""
        );
    }

    #[test]
    fn a_starred_object_offers_to_unstar() {
        let s = snap();
        let favs = vec![FavouriteObject {
            connection_id: "c1".into(),
            database: None,
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: "orders".into(),
        }];
        let m = menu_for(&table_row(), &MenuCtx {
            read_only: false,
            dialect: Some(Dialect::Mssql),
            snapshot: Some(&s),
            favourites: &favs,
            conn_id: "c1",
            database: None,
        });
        assert!(labels(&m).contains(&"Odebrat z oblíbených".to_string()));
    }

    /// A database row whose schema snapshot is loaded and holds exactly one
    /// schema offers the ER diagram FOR THAT SCHEMA.
    ///
    /// Regression: this used to emit `schema: None` unconditionally, which
    /// `AppView::open_er_diagram` filters the snapshot by — on MSSQL every
    /// table is `Some("dbo")`, so the filter matched nothing and the tab
    /// opened empty. It looked to the user like the click did nothing at
    /// all, which is why the assert is on the payload and not on the label.
    #[test]
    fn er_diagram_from_a_database_row_names_the_schema_it_will_draw() {
        let s = snap();
        let mut c = ctx(&s, false);
        c.database = Some("prod".into());
        let row = SidebarRow::Database { conn_id: "c1".into(), db: "prod".into() };
        let m = menu_for(&row, &c);
        let ev = m.iter().find_map(|e| match e {
            MenuEntry::Item(i) if i.label.starts_with("ER diagram") => Some(i.event.clone()),
            _ => None,
        });
        assert_eq!(ev, Some(TreeEvent::OpenErDiagram { schema: Some("dbo".into()) }));
    }

    /// SQLite has no schemas: `Some(None)` is a real answer, not a refusal.
    #[test]
    fn er_diagram_from_a_schemaless_database_row_passes_none() {
        let mut s = SchemaSnapshot::default();
        s.tables.push(TableInfo { schema: None, name: "t".into(), ..Default::default() });
        let mut c = ctx(&s, false);
        c.database = Some("main".into());
        let row = SidebarRow::Database { conn_id: "c1".into(), db: "main".into() };
        let m = menu_for(&row, &c);
        assert!(m.iter().any(
            |e| matches!(e, MenuEntry::Item(i) if i.event == TreeEvent::OpenErDiagram { schema: None })
        ));
    }

    /// Three ways the question „which schema?" has no single answer. Each
    /// must omit the item — an ER entry that draws the wrong database, or
    /// nothing at all, is worse than no entry.
    #[test]
    fn a_database_row_offers_no_er_diagram_when_the_schema_is_ambiguous() {
        let s = snap();

        // (a) not the active database — the snapshot is another database's.
        let mut other = ctx(&s, false);
        other.database = Some("prod".into());
        let row = SidebarRow::Database { conn_id: "c1".into(), db: "staging".into() };
        assert!(!labels(&menu_for(&row, &other)).iter().any(|l| l.starts_with("ER diagram")));

        // (b) no snapshot loaded yet.
        let mut unloaded = ctx(&s, false);
        unloaded.snapshot = None;
        unloaded.database = Some("prod".into());
        let row = SidebarRow::Database { conn_id: "c1".into(), db: "prod".into() };
        assert!(!labels(&menu_for(&row, &unloaded)).iter().any(|l| l.starts_with("ER diagram")));

        // (c) several schemas — the schema rows own that case.
        let mut multi = snap();
        multi.tables.push(TableInfo {
            schema: Some("sales".into()),
            name: "leads".into(),
            ..Default::default()
        });
        let mut c = ctx(&multi, false);
        c.database = Some("prod".into());
        let row = SidebarRow::Database { conn_id: "c1".into(), db: "prod".into() };
        assert!(!labels(&menu_for(&row, &c)).iter().any(|l| l.starts_with("ER diagram")));
    }

    /// The separator shape has to hold with the ER item present too — the
    /// row above pushes it in conditionally.
    #[test]
    fn an_active_database_row_still_has_a_well_formed_menu() {
        let s = snap();
        let mut c = ctx(&s, false);
        c.database = Some("prod".into());
        let m = menu_for(&SidebarRow::Database { conn_id: "c1".into(), db: "prod".into() }, &c);
        assert!(matches!(m.first(), Some(MenuEntry::Item(_))));
        assert!(matches!(m.last(), Some(MenuEntry::Item(_))));
        assert!(
            !m.windows(2)
                .any(|w| matches!((&w[0], &w[1]), (MenuEntry::Separator, MenuEntry::Separator)))
        );
    }

    /// A folder row used to have no menu at all — folders were implicit and
    /// there was nothing to do to one.
    #[test]
    fn a_folder_offers_the_four_things_you_can_do_to_a_folder() {
        let sn = snap();
        let row = SidebarRow::Folder { path: vec!["work".into()] };
        let m = menu_for(&row, &ctx(&sn, false));
        let l = labels(&m);
        assert!(l.iter().any(|x| x.starts_with("Nová podsložka")), "{l:?}");
        assert!(l.iter().any(|x| x.starts_with("Přejmenovat")), "{l:?}");
        assert!(l.iter().any(|x| x.starts_with("Smazat složku")), "{l:?}");
    }

    /// A folder is where connections live, so making one is what a right
    /// click there is most often for — and it must be the FIRST row, not
    /// buried under the folder-management ones (user report, 2026-09-01).
    /// The folder travels with the event so the dialog opens with it
    /// already filled in.
    #[test]
    fn a_folder_offers_making_a_connection_in_it_first() {
        let sn = snap();
        let row = SidebarRow::Folder { path: vec!["work".into(), "dw".into()] };
        let m = menu_for(&row, &ctx(&sn, false));
        assert!(labels(&m)[0].starts_with("Nové připojení"), "{:?}", labels(&m));
        let MenuEntry::Item(first) = &m[0] else { panic!("first entry is a separator") };
        assert_eq!(
            first.event,
            TreeEvent::ConnectionCreate { folder: vec!["work".into(), "dw".into()] },
            "the folder must travel with the event or the dialog opens at the root"
        );
    }

    /// Same reason `a_read_only_connection_can_still_organise_its_folders`
    /// exists: saving a connection writes `config.toml`, never the server.
    #[test]
    fn a_read_only_connection_can_still_make_a_connection_in_a_folder() {
        let sn = snap();
        let row = SidebarRow::Folder { path: vec!["work".into()] };
        let l = labels(&menu_for(&row, &ctx(&sn, true)));
        assert!(l.iter().any(|x| x.starts_with("Nové připojení")), "{l:?}");
    }

    /// Folders hold saved connections, not server objects — the read-only
    /// flag is about the SERVER and must not disable tidying up locally.
    #[test]
    fn a_read_only_connection_can_still_organise_its_folders() {
        let sn = snap();
        let row = SidebarRow::Folder { path: vec!["work".into()] };
        assert_eq!(menu_for(&row, &ctx(&sn, true)).len(), menu_for(&row, &ctx(&sn, false)).len());
    }

    /// Dropping a database is intentionally not offered here — „Správa
    /// serveru" owns it, with its CASCADE warning.
    #[test]
    fn a_database_row_never_offers_to_drop_the_database() {
        let s = snap();
        let row = SidebarRow::Database { conn_id: "c1".into(), db: "prod".into() };
        let m = menu_for(&row, &ctx(&s, false));
        assert!(!labels(&m).iter().any(|l| l.contains("DROP")), "{:?}", labels(&m));
        assert!(m.iter().all(|e| !matches!(e, MenuEntry::Item(i) if i.danger)));
    }

    #[test]
    fn rows_that_are_not_objects_have_no_menu() {
        let s = snap();
        for row in [
            SidebarRow::Notice {
                conn_id: "c1".into(),
                db: None,
                text: "Načítám…".into(),
                retry: false,
            },
            SidebarRow::ScriptNotice { text: "x".into(), open_settings: false },
        ] {
            assert!(menu_for(&row, &ctx(&s, false)).is_empty(), "{row:?} should have no menu");
        }
    }

    /// Every menu that has items must start and end with an item, never a
    /// separator, and must never show two separators in a row — the shapes
    /// that look like rendering bugs.
    #[test]
    fn no_menu_has_dangling_or_doubled_separators() {
        let s = snap();
        let rows = [
            table_row(),
            SidebarRow::Connection { conn_id: "c1".into() },
            SidebarRow::Database { conn_id: "c1".into(), db: "d".into() },
            SidebarRow::Inner {
                conn_id: "c1".into(),
                db: "d".into(),
                node: NodeId::Schema("dbo".into()),
            },
            SidebarRow::Inner {
                conn_id: "c1".into(),
                db: "d".into(),
                node: NodeId::Column("dbo".into(), "orders".into(), "id".into()),
            },
            SidebarRow::ScriptFile { rel: "a.sql".into() },
            SidebarRow::ScriptFolder { rel: "sub".into() },
            SidebarRow::ScriptsRoot,
        ];
        for read_only in [false, true] {
            for row in &rows {
                let m = menu_for(row, &ctx(&s, read_only));
                if m.is_empty() {
                    continue;
                }
                assert!(
                    matches!(m.first(), Some(MenuEntry::Item(_))),
                    "{row:?} (ro={read_only}) starts with a separator"
                );
                assert!(
                    matches!(m.last(), Some(MenuEntry::Item(_))),
                    "{row:?} (ro={read_only}) ends with a separator"
                );
                for pair in m.windows(2) {
                    assert!(
                        !matches!((&pair[0], &pair[1]), (MenuEntry::Separator, MenuEntry::Separator)),
                        "{row:?} (ro={read_only}) has doubled separators"
                    );
                }
            }
        }
    }

    /// „Smazat připojení…" is offered on a connection row in BOTH read-only
    /// states, and NOWHERE else. Read-only is a promise about the SERVER;
    /// suppressing it there (the reflex, since every other `danger` item is
    /// suppressed) would leave a read-only connection impossible to remove.
    /// The database row is the negative half: „delete" there would read as
    /// DROP DATABASE, which this menu deliberately does not offer at all.
    #[test]
    fn a_connection_can_be_deleted_read_only_or_not_and_a_database_cannot() {
        let s = snap();
        for read_only in [false, true] {
            let conn = labels(&menu_for(&SidebarRow::Connection { conn_id: "c1".into() }, &ctx(&s, read_only)));
            assert!(
                conn.iter().any(|l| l.starts_with("Smazat připojení")),
                "ro={read_only}: {conn:?}"
            );
            let db = labels(&menu_for(
                &SidebarRow::Database { conn_id: "c1".into(), db: "d".into() },
                &ctx(&s, read_only),
            ));
            assert!(!db.iter().any(|l| l.starts_with("Smazat")), "ro={read_only}: {db:?}");
        }
    }
}
