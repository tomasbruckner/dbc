// G2 Task 6: schema tree panel + speed search.
//
// Layout of this file:
//   1. `NodeId` — stable, path-based identity for every row (survives a
//      refresh: re-fetching the same schema and re-flattening produces the
//      same ids for unchanged objects, so `expanded`/`selected` don't reset
//      just because the underlying `SchemaSnapshot` was replaced).
//   2. `flatten` (+ its private per-kind `emit_*` helpers) — GPUI-free pure
//      logic that turns a `SchemaSnapshot` + `expanded` set + `filter`
//      string into the exact list of VISIBLE rows to render this frame.
//      Unit-tested directly below, with no GPUI dependency at all.
//   3. `SchemaTree` — the GPUI entity. Sidebar rework (T5): a multi-root
//      sidebar — every saved connection is a root, expanding into its
//      databases, each database into its own schema slot (`ConnNode`/
//      `DbListState`/`DbSchemaState`); renders via `uniform_list` (calling
//      `flatten_sidebar` fresh every frame — brief contract #2), and emits
//      `TreeEvent`s for the things it can't handle itself (opening a
//      preview/DDL tab, fetching a db list/schema slot, switching the
//      active database).
//
// Fetch-lifecycle state is driven by direct entity mutation from `main.rs`
// (`begin_db_list`/`finish_db_list`/`begin_schema`/`finish_schema`), with
// the FETCH REQUESTS themselves riding `TreeEvent::{LoadDatabases,
// LoadSchema}` — `main.rs` owns the `QueryRunner`, the vault gate and the
// spec resolution, so it remains the owner of "start a fetch, update the
// tree entity when it resolves" (see `AppView::start_db_list_fetch`/
// `start_schema_slot_fetch`).

use std::collections::{BTreeSet, HashMap, HashSet};

use dbc_core::{
    synthesize_create_table, ColumnInfo, RoutineInfo, RoutineKind, SchemaSnapshot, SequenceInfo,
    TableInfo, TableKind, TriggerInfo,
};
use dbc_state::{ConnectionConfig, Engine, FavouriteObject};

use crate::admin_panel::AdminEntry;
use gpui::{
    actions, div, prelude::*, px, uniform_list, App, ClickEvent, Context, EventEmitter,
    FocusHandle, Focusable, KeyBinding, KeyDownEvent, MouseButton, Window,
};

use crate::theme::ActiveTheme;

/// Shown in place of a routine/trigger's DDL when the driver didn't provide
/// one (`RoutineInfo::ddl`/`TriggerInfo::ddl` is `None`).
pub const DDL_FALLBACK: &str = "-- DDL není k dispozici";

/// Stable, path-based node identity — NOT index-based, so `expanded`,
/// `selected`, and speed-search filtering stay meaningful across a refresh
/// that replaces `snapshot` wholesale. The `String` fields hold the schema
/// name, with SQLite's `schema: None` normalized to `""` (see
/// `schema_key_string`) so ids stay stable regardless of whether the schema
/// level itself is rendered (single-implicit-level omission, contract #3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeId {
    Schema(String),
    /// (schema, "Tabulky"|"Pohledy"|"Funkce"|"Procedury"|"Triggery"|"Indexy"|"Sekvence")
    Section(String, &'static str),
    /// (schema, name) — also views/materialized views.
    Table(String, String),
    /// (schema, table, column)
    Column(String, String, String),
    /// (schema, name)
    Routine(String, String),
    /// (schema, name)
    Trigger(String, String),
    /// (schema, name)
    Sequence(String, String),
    /// (schema, table, index)
    Index(String, String, String),
    /// The top "Oblíbené" section header (G3 Task 4) — a single, unparameterized
    /// variant since there's only ever one such section, always rendered at
    /// depth 0, before any `Schema`/`Section` node.
    FavouriteSection,
    /// (kind, schema key string ("" = no schema), name) — one row under
    /// `FavouriteSection`. `kind` mirrors `FavouriteObject::kind`
    /// ("table"|"view"|"routine"|"trigger"|"sequence").
    Favourite(String, String, String),
    /// G10 T4: the "Správa serveru" pinned entry row (design §2 "rendered
    /// above Favourites, not a real catalog object") — a single,
    /// unparameterized variant, same shape as `FavouriteSection`. Never
    /// expandable; a click emits `TreeEvent::OpenAdmin` when the tree's
    /// `admin_entry` is `Enabled` (see `flatten`/`SchemaTree::render`).
    AdminRoot,
}

/// Emitted by `SchemaTree` (`EventEmitter<TreeEvent>`) for the things it
/// can't act on itself — `main.rs` subscribes and handles them.
pub enum TreeEvent {
    /// WIDENED (sidebar rework, design §5 row 1): carries the scope of the
    /// row that emitted it, so `main.rs` can switch-then-open across
    /// contexts (an inactive-scope double-click queues the open and
    /// switches first).
    OpenPreview { conn_id: String, db: String, schema: Option<String>, table: String },
    OpenDdl { title: String, ddl: String },
    /// Targets the ACTIVE `(connection, database)` slot (sidebar rework —
    /// the ⟳ header button's semantics are unchanged from the single-root
    /// era: refresh what the editor is talking to).
    RefreshRequested,
    /// G3 Task 4: the row's ★/☆ toggle was clicked (either a table/view/
    /// routine/trigger/sequence row, or an item already listed under
    /// `FavouriteSection`) — `main.rs` applies `config.toggle_favourite` +
    /// a guarded save, then pushes the updated set back via `set_favourites`.
    ToggleFavourite(FavouriteObject),
    /// G8 T6: the "⊞" icon on a `NodeId::Schema(_)` row was clicked —
    /// `main.rs` opens (or re-scopes) the ER diagram tab for this schema.
    OpenErDiagram { schema: Option<String> },
    /// G12 T4: the "⇪" icon on a `NodeId::Table(_, _)` row was clicked —
    /// `main.rs` starts the CSV-import file-picker/pre-count/mapping flow
    /// for this table. Never emitted while the tree is `read_only` (the
    /// icon isn't rendered at all in that state — see `read_only`'s doc
    /// comment).
    ImportCsv { schema: Option<String>, table: String },
    /// G10 T4: the pinned "Správa serveru" row was clicked while `Enabled`
    /// — `main.rs::open_admin_tab` re-checks `admin_entry_state` itself
    /// defensively (belt-and-braces with the runner's shared read-only
    /// guard).
    OpenAdmin,
    /// Sidebar rework: expand (or error-row retry) of a Connection row
    /// whose database list is `NotLoaded`/`Error` — `main.rs` dispatches
    /// `fetch_database_list` (vault-gated).
    LoadDatabases { conn_id: String },
    /// Sidebar rework: expand (or error-row retry) of a Database row whose
    /// schema slot is `NotLoaded`/`Error` — `main.rs` dispatches
    /// `fetch_schema` for that `(conn, db)` slot (vault-gated, design §4.4).
    LoadSchema { conn_id: String, db: String },
    /// Sidebar rework (design §2.1): double-click on a Database row
    /// (`db: Some(..)`) or a Connection row (`db: None` = the saved
    /// default) — `main.rs::switch_to_database` performs the context
    /// switch. Expanding (chevron) never emits this: browsing ≠ switching.
    SwitchToDatabase { conn_id: String, db: Option<String> },
}

/// One visible row: `(id, depth, label, is_expandable)`.
pub type FlatNode = (NodeId, usize, String, bool);

fn schema_key_string(schema: &Option<String>) -> String {
    schema.clone().unwrap_or_default()
}

/// Case-insensitive substring test against an already-lowercased `filter`;
/// an empty filter matches everything (filtering is inactive).
fn name_matches(name: &str, filter_lc: &str) -> bool {
    filter_lc.is_empty() || name.to_lowercase().contains(filter_lc)
}

/// A table/view "matches" the filter if its own name does, or any of its
/// columns' names do — this is what makes a table with only a matching
/// *column* still show up (ancestors auto-show, brief contract #5).
fn table_subtree_matches(t: &TableInfo, filter_lc: &str) -> bool {
    name_matches(&t.name, filter_lc) || t.columns.iter().any(|c| name_matches(&c.name, filter_lc))
}

fn schema_subtree_matches(snapshot: &SchemaSnapshot, schema_key: &Option<String>, filter_lc: &str) -> bool {
    snapshot.tables.iter().any(|t| &t.schema == schema_key && table_subtree_matches(t, filter_lc))
        || snapshot.routines.iter().any(|r| &r.schema == schema_key && name_matches(&r.name, filter_lc))
        || snapshot.triggers.iter().any(|t| &t.schema == schema_key && name_matches(&t.name, filter_lc))
        || snapshot.sequences.iter().any(|s| &s.schema == schema_key && name_matches(&s.name, filter_lc))
        || snapshot.tables.iter().any(|t| {
            &t.schema == schema_key
                && t.indexes.iter().any(|idx| name_matches(&format!("{}.{}", t.name, idx.name), filter_lc))
        })
}

/// While filtering, every node on the path to a match is treated as
/// expanded regardless of the manual `expanded` set — otherwise a matching
/// column buried under a collapsed table would never become visible. Nodes
/// (and whole branches) that don't lead to any match are skipped entirely
/// by the `emit_*` helpers below, via their own `*_matches` filtering, not
/// by this function.
fn is_expanded(expanded: &HashSet<NodeId>, filter_active: bool, id: &NodeId) -> bool {
    filter_active || expanded.contains(id)
}

/// Column label per brief contract #3: `"{name}: {type}"` + ` PK` / ` FK` /
/// ` ?` (nullable) markers + ` = {default}` when present, in that order.
fn column_label(c: &ColumnInfo) -> String {
    let mut s = format!("{}: {}", c.name, c.data_type);
    if c.is_pk {
        s.push_str(" PK");
    }
    if c.fk.is_some() {
        s.push_str(" FK");
    }
    if c.nullable {
        s.push_str(" ?");
    }
    if let Some(d) = &c.default {
        s.push_str(&format!(" = {d}"));
    }
    s
}

/// Renders one Tabulky/Pohledy-style section: a `Section` header (skipped
/// entirely when the filtered item list is empty — brief contract #3 "only
/// non-empty sections render") followed, if expanded, by one `Table` row
/// per item and, if that table is itself expanded, one `Column` row per
/// column (filtered to just the matching columns unless the table's own
/// name matched, in which case all of its columns show).
fn emit_table_like_section(
    out: &mut Vec<FlatNode>,
    schema_name: &str,
    label: &'static str,
    items: Vec<&TableInfo>,
    depth: usize,
    expanded: &HashSet<NodeId>,
    filter_lc: &str,
    filter_active: bool,
) {
    let filtered: Vec<&TableInfo> =
        items.into_iter().filter(|t| !filter_active || table_subtree_matches(t, filter_lc)).collect();
    if filtered.is_empty() {
        return;
    }
    let section_id = NodeId::Section(schema_name.to_string(), label);
    out.push((section_id.clone(), depth, format!("{label} ({})", filtered.len()), true));
    if !is_expanded(expanded, filter_active, &section_id) {
        return;
    }
    for t in filtered {
        let table_id = NodeId::Table(schema_name.to_string(), t.name.clone());
        out.push((table_id.clone(), depth + 1, t.name.clone(), true));
        if !is_expanded(expanded, filter_active, &table_id) {
            continue;
        }
        let table_self_matches = name_matches(&t.name, filter_lc);
        for c in &t.columns {
            if filter_active && !table_self_matches && !name_matches(&c.name, filter_lc) {
                continue;
            }
            let col_id = NodeId::Column(schema_name.to_string(), t.name.clone(), c.name.clone());
            out.push((col_id, depth + 2, column_label(c), false));
        }
    }
}

fn routine_label(r: &RoutineInfo) -> String {
    if r.signature.is_empty() {
        r.name.clone()
    } else {
        format!("{}{}", r.name, r.signature)
    }
}

fn emit_routine_section(
    out: &mut Vec<FlatNode>,
    schema_name: &str,
    label: &'static str,
    items: Vec<&RoutineInfo>,
    depth: usize,
    expanded: &HashSet<NodeId>,
    filter_lc: &str,
    filter_active: bool,
) {
    let filtered: Vec<&RoutineInfo> =
        items.into_iter().filter(|r| !filter_active || name_matches(&r.name, filter_lc)).collect();
    if filtered.is_empty() {
        return;
    }
    let section_id = NodeId::Section(schema_name.to_string(), label);
    out.push((section_id.clone(), depth, format!("{label} ({})", filtered.len()), true));
    if !is_expanded(expanded, filter_active, &section_id) {
        return;
    }
    for r in filtered {
        let id = NodeId::Routine(schema_name.to_string(), r.name.clone());
        out.push((id, depth + 1, routine_label(r), false));
    }
}

fn emit_trigger_section(
    out: &mut Vec<FlatNode>,
    schema_name: &str,
    triggers: Vec<&TriggerInfo>,
    depth: usize,
    expanded: &HashSet<NodeId>,
    filter_lc: &str,
    filter_active: bool,
) {
    let filtered: Vec<&TriggerInfo> =
        triggers.into_iter().filter(|t| !filter_active || name_matches(&t.name, filter_lc)).collect();
    if filtered.is_empty() {
        return;
    }
    let section_id = NodeId::Section(schema_name.to_string(), "Triggery");
    out.push((section_id.clone(), depth, format!("Triggery ({})", filtered.len()), true));
    if !is_expanded(expanded, filter_active, &section_id) {
        return;
    }
    for t in filtered {
        let id = NodeId::Trigger(schema_name.to_string(), t.name.clone());
        out.push((id, depth + 1, format!("{} ({})", t.name, t.table), false));
    }
}

/// Flattens indexes from every table under `schema_key` into a single flat
/// list, labeled `"{table}.{index}"` (brief contract #3 — no per-table
/// grouping node for this section).
fn emit_index_section(
    out: &mut Vec<FlatNode>,
    schema_name: &str,
    schema_key: &Option<String>,
    snapshot: &SchemaSnapshot,
    depth: usize,
    expanded: &HashSet<NodeId>,
    filter_lc: &str,
    filter_active: bool,
) {
    let mut items: Vec<(String, String)> = Vec::new();
    for t in snapshot.tables.iter().filter(|t| &t.schema == schema_key) {
        for idx in &t.indexes {
            items.push((t.name.clone(), idx.name.clone()));
        }
    }
    let filtered: Vec<(String, String)> = items
        .into_iter()
        .filter(|(t, i)| !filter_active || name_matches(&format!("{t}.{i}"), filter_lc))
        .collect();
    if filtered.is_empty() {
        return;
    }
    let section_id = NodeId::Section(schema_name.to_string(), "Indexy");
    out.push((section_id.clone(), depth, format!("Indexy ({})", filtered.len()), true));
    if !is_expanded(expanded, filter_active, &section_id) {
        return;
    }
    for (t, i) in filtered {
        let id = NodeId::Index(schema_name.to_string(), t.clone(), i.clone());
        out.push((id, depth + 1, format!("{t}.{i}"), false));
    }
}

fn emit_sequence_section(
    out: &mut Vec<FlatNode>,
    schema_name: &str,
    seqs: Vec<&SequenceInfo>,
    depth: usize,
    expanded: &HashSet<NodeId>,
    filter_lc: &str,
    filter_active: bool,
) {
    let filtered: Vec<&SequenceInfo> =
        seqs.into_iter().filter(|s| !filter_active || name_matches(&s.name, filter_lc)).collect();
    if filtered.is_empty() {
        return;
    }
    let section_id = NodeId::Section(schema_name.to_string(), "Sekvence");
    out.push((section_id.clone(), depth, format!("Sekvence ({})", filtered.len()), true));
    if !is_expanded(expanded, filter_active, &section_id) {
        return;
    }
    for s in filtered {
        let id = NodeId::Sequence(schema_name.to_string(), s.name.clone());
        out.push((id, depth + 1, s.name.clone(), false));
    }
}

/// Emits the top "Oblíbené" section (G3 Task 4 brief contract): before any
/// schema/section, listing favourited objects of the ACTIVE connection only
/// (cross-schema — `favourites` is `AppConfig::favourite_objects` unfiltered
/// by kind or schema, just by `connection_id`), labeled `"{schema}.{name}"`
/// when the favourite has a schema, else just `"{name}"`. Hidden entirely
/// (no header, no rows) when there are none — either because
/// `active_connection_id` is `None` (no active connection / CLI-arg URL path
/// with no id to match against) or because none of `favourites` belong to
/// it. Unlike the schema/section trees, this section's expand state is not
/// forced open by an active speed-search filter — favourites are a small,
/// flat, orthogonal-to-schema list, not something a filter needs to reach
/// into.
/// Emits all seven sections (fixed order: Tabulky, Pohledy, Funkce,
/// Procedury, Triggery, Indexy, Sekvence) for one schema, at `depth`.
fn emit_sections(
    out: &mut Vec<FlatNode>,
    snapshot: &SchemaSnapshot,
    schema_key: &Option<String>,
    depth: usize,
    expanded: &HashSet<NodeId>,
    filter_lc: &str,
    filter_active: bool,
) {
    let schema_name = schema_key_string(schema_key);

    let tables: Vec<&TableInfo> =
        snapshot.tables.iter().filter(|t| &t.schema == schema_key && t.kind == TableKind::Table).collect();
    emit_table_like_section(out, &schema_name, "Tabulky", tables, depth, expanded, filter_lc, filter_active);

    let views: Vec<&TableInfo> = snapshot
        .tables
        .iter()
        .filter(|t| &t.schema == schema_key && matches!(t.kind, TableKind::View | TableKind::MaterializedView))
        .collect();
    emit_table_like_section(out, &schema_name, "Pohledy", views, depth, expanded, filter_lc, filter_active);

    let funcs: Vec<&RoutineInfo> = snapshot
        .routines
        .iter()
        .filter(|r| &r.schema == schema_key && r.kind == RoutineKind::Function)
        .collect();
    emit_routine_section(out, &schema_name, "Funkce", funcs, depth, expanded, filter_lc, filter_active);

    let procs: Vec<&RoutineInfo> = snapshot
        .routines
        .iter()
        .filter(|r| &r.schema == schema_key && r.kind == RoutineKind::Procedure)
        .collect();
    emit_routine_section(out, &schema_name, "Procedury", procs, depth, expanded, filter_lc, filter_active);

    let triggers: Vec<&TriggerInfo> = snapshot.triggers.iter().filter(|t| &t.schema == schema_key).collect();
    emit_trigger_section(out, &schema_name, triggers, depth, expanded, filter_lc, filter_active);

    emit_index_section(out, &schema_name, schema_key, snapshot, depth, expanded, filter_lc, filter_active);

    let seqs: Vec<&SequenceInfo> = snapshot.sequences.iter().filter(|s| &s.schema == schema_key).collect();
    emit_sequence_section(out, &schema_name, seqs, depth, expanded, filter_lc, filter_active);
}

/// Sidebar rework (T4): the schema-only core of the old `flatten` (deleted
/// in T5 — `flatten_sidebar` owns the pinned admin/favourites rows now,
/// design §1.1). Schema grouping (contract #3): when every
/// table/routine/trigger/sequence in `snapshot` has `schema: None` (SQLite
/// has no schema concept), the schema level is a single implicit level and
/// is omitted entirely — sections render straight at depth 0. Otherwise
/// each distinct schema gets its own expandable `Schema` node at depth 0,
/// with sections nested one level deeper once that schema is expanded.
///
/// The schema-key collection, the `single_implicit` decision, and the
/// per-schema `emit_sections` loop were extracted verbatim so
/// `flatten_sidebar` can splice one database's rows at an arbitrary depth
/// without dragging the pinned admin/favourites rows along. Takes the RAW
/// filter and lowercases
/// internally (same convention as `flatten`, which it inherited the body
/// from). Pure, GPUI-free; never fetches.
pub fn flatten_schema(
    snapshot: &SchemaSnapshot,
    expanded: &HashSet<NodeId>,
    filter: &str,
) -> Vec<FlatNode> {
    let mut out = Vec::new();
    let filter_lc = filter.to_lowercase();
    let filter_active = !filter_lc.is_empty();

    let mut schema_key_set: BTreeSet<Option<String>> = BTreeSet::new();
    for t in &snapshot.tables {
        schema_key_set.insert(t.schema.clone());
    }
    for r in &snapshot.routines {
        schema_key_set.insert(r.schema.clone());
    }
    for tr in &snapshot.triggers {
        schema_key_set.insert(tr.schema.clone());
    }
    for s in &snapshot.sequences {
        schema_key_set.insert(s.schema.clone());
    }
    let schema_keys: Vec<Option<String>> = schema_key_set.into_iter().collect();

    let single_implicit = schema_keys.iter().all(|k| k.is_none());

    if single_implicit {
        let key = schema_keys.into_iter().next().unwrap_or(None);
        emit_sections(&mut out, snapshot, &key, 0, expanded, &filter_lc, filter_active);
    } else {
        for key in schema_keys {
            if filter_active && !schema_subtree_matches(snapshot, &key, &filter_lc) {
                continue;
            }
            let schema_name = schema_key_string(&key);
            let node = NodeId::Schema(schema_name.clone());
            out.push((node.clone(), 0, schema_name, true));
            if is_expanded(expanded, filter_active, &node) {
                emit_sections(&mut out, snapshot, &key, 1, expanded, &filter_lc, filter_active);
            }
        }
    }
    out
}

/// Pure, GPUI-free: every `NodeId` that could possibly appear for
/// `snapshot`, regardless of `expanded`/`filter` (unlike `flatten`, which
/// only returns the currently-VISIBLE rows). Used by `prune_stale_ids` to
/// tell which previously-expanded/selected ids still refer to something
/// real after a same-connection refresh replaces `snapshot` wholesale.
fn all_node_ids(snapshot: &SchemaSnapshot) -> HashSet<NodeId> {
    let mut out = HashSet::new();
    // G3 Task 4: always valid, independent of `snapshot` — its favourites
    // come from config, not the schema fetch — so a same-connection refresh
    // never drops the section's expand state out from under the user.
    out.insert(NodeId::FavouriteSection);

    let mut schema_key_set: BTreeSet<Option<String>> = BTreeSet::new();
    for t in &snapshot.tables {
        schema_key_set.insert(t.schema.clone());
    }
    for r in &snapshot.routines {
        schema_key_set.insert(r.schema.clone());
    }
    for tr in &snapshot.triggers {
        schema_key_set.insert(tr.schema.clone());
    }
    for s in &snapshot.sequences {
        schema_key_set.insert(s.schema.clone());
    }
    let schema_keys: Vec<Option<String>> = schema_key_set.into_iter().collect();
    let single_implicit = schema_keys.iter().all(|k| k.is_none());

    for key in &schema_keys {
        let schema_name = schema_key_string(key);
        if !single_implicit {
            out.insert(NodeId::Schema(schema_name.clone()));
        }

        for t in snapshot.tables.iter().filter(|t| &t.schema == key) {
            let section_label = match t.kind {
                TableKind::Table => "Tabulky",
                TableKind::View | TableKind::MaterializedView => "Pohledy",
            };
            out.insert(NodeId::Section(schema_name.clone(), section_label));
            out.insert(NodeId::Table(schema_name.clone(), t.name.clone()));
            for c in &t.columns {
                out.insert(NodeId::Column(schema_name.clone(), t.name.clone(), c.name.clone()));
            }
            if !t.indexes.is_empty() {
                out.insert(NodeId::Section(schema_name.clone(), "Indexy"));
            }
            for idx in &t.indexes {
                out.insert(NodeId::Index(schema_name.clone(), t.name.clone(), idx.name.clone()));
            }
        }
        for r in snapshot.routines.iter().filter(|r| &r.schema == key) {
            let section_label = match r.kind {
                RoutineKind::Function => "Funkce",
                RoutineKind::Procedure => "Procedury",
            };
            out.insert(NodeId::Section(schema_name.clone(), section_label));
            out.insert(NodeId::Routine(schema_name.clone(), r.name.clone()));
        }
        for tr in snapshot.triggers.iter().filter(|tr| &tr.schema == key) {
            out.insert(NodeId::Section(schema_name.clone(), "Triggery"));
            out.insert(NodeId::Trigger(schema_name.clone(), tr.name.clone()));
        }
        for s in snapshot.sequences.iter().filter(|s| &s.schema == key) {
            out.insert(NodeId::Section(schema_name.clone(), "Sekvence"));
            out.insert(NodeId::Sequence(schema_name.clone(), s.name.clone()));
        }
    }
    out
}

/// Pure, GPUI-free: computes the `expanded`/`selected` state to carry
/// forward into a same-connection refresh, dropping any id that no longer
/// exists in `new_snapshot` (e.g. a table dropped since the last fetch).
/// Extracted out of `SchemaTree::set_snapshot` specifically so it's
/// unit-testable without a GPUI `Context`.
fn prune_stale_ids(
    expanded: &HashSet<NodeId>,
    selected: &Option<NodeId>,
    new_snapshot: &SchemaSnapshot,
) -> (HashSet<NodeId>, Option<NodeId>) {
    let valid = all_node_ids(new_snapshot);
    let expanded = expanded.iter().filter(|id| valid.contains(*id)).cloned().collect();
    let selected = selected.clone().filter(|id| valid.contains(id));
    (expanded, selected)
}

// ---------------------------------------------------------------------
// Sidebar rework (T4 state layer, wired into the entity/render by T5 —
// the multi-root sidebar IS the UI now; the T4-era `#[allow(dead_code)]`
// markers are gone with their owner's arrival).
// ---------------------------------------------------------------------

/// One row of the multi-root sidebar. `NodeId` itself is UNCHANGED
/// (path-stable within one database) — the `(conn_id, db)` scope travels
/// ALONGSIDE it in this wrapper, not inside it (design §1.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SidebarRow {
    Folder { path: Vec<String> },
    Connection { conn_id: String },
    Database { conn_id: String, db: String },
    Inner { conn_id: String, db: String, node: NodeId },
    /// Pinned active-context rows: AdminRoot, FavouriteSection, Favourite.
    /// Reuses the existing `NodeId` values so their click/double-click
    /// semantics stay the pre-rework code paths verbatim (resolved
    /// deviation 14).
    Pinned(NodeId),
    /// "Načítám…"/error/truncation rows. `retry` = a click re-emits the
    /// Load event (db == None → LoadDatabases, Some → LoadSchema).
    Notice { conn_id: String, db: Option<String>, text: String, retry: bool },
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
}

/// Expand-state key for the OUTER (multi-root) levels — the inner
/// per-database `expanded: HashSet<NodeId>` lives in each `DbSchemaState`
/// slot.
///
/// POLARITY ASYMMETRY: presence of `Folder(_)` in the set means COLLAPSED
/// (folders default OPEN — the pre-rework dropdown showed everything;
/// storing the exception keeps old sessions looking unchanged), while
/// presence of `Connection(_)`/`Database(..)`/`Favourites` means EXPANDED
/// (they default CLOSED — lazy).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OuterId {
    Folder(Vec<String>),
    Connection(String),
    Database(String, String),
    Favourites,
    /// The „Skripty" section itself — LAZY polarity (presence = expanded),
    /// like `Connection`/`Database`/`Favourites`, NOT the inverted
    /// `Folder` polarity: the section is collapsed by default (Part S §1.4).
    Scripts,
    /// One scripts-library folder, keyed by its '/'-separated `rel`. Lazy
    /// polarity too — the scripts tree is browsed, not pre-opened.
    ScriptFolder(String),
}

/// The active `(connection, database)` context as the sidebar sees it —
/// handed in by `main.rs` (T5) from `resolve_active`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveScope {
    pub conn_id: String,
    /// The EFFECTIVE database (spec-level string).
    pub db: String,
    /// The saved config's database — resolves `FavouriteObject::database:
    /// None` (design §5 row 9).
    pub default_db: String,
}

/// Per-connection sidebar state: the lazily fetched database list, each
/// entry carrying its own lazily fetched schema slot.
pub struct ConnNode {
    pub dbs: DbListState,
}

/// Lazy-fetch state machine for one connection's database list (design
/// §1.2): `generation` guards against a stale in-flight result clobbering a
/// newer dispatch (`apply_db_list_result` drops mismatches).
pub enum DbListState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { dbs: Vec<DbNode>, truncated: bool },
}

/// Lazy-scan state machine for the scripts library (Part S §3.3) — the same
/// family as `DbListState`, `generation` guarding against a stale in-flight
/// scan clobbering a newer dispatch. There is exactly ONE of these on
/// `SchemaTree`: the library is global, not per-connection (Part S §1.1).
pub enum ScriptsListState {
    NotLoaded,
    Loading {
        /// The dispatch this `Loading` belongs to. Written by
        /// `begin_scripts_scan`; the staleness comparison itself reads
        /// `SchemaTree::scripts_generation`, so nothing reads this copy
        /// until the flip. DARK UNTIL TASK 7 — removal owner: Task 7 (the
        /// scripts flip), whose sidebar render surfaces it.
        #[allow(dead_code)]
        generation: u64,
    },
    Error(String),
    Loaded { entries: Vec<crate::scripts::ScriptEntry>, truncated: bool, depth_clipped: bool },
}

/// One database under a connection. `name` is the SPEC-LEVEL string (full
/// file path for file engines — resolved deviation 5; `display_db_name`
/// renders the stem).
pub struct DbNode {
    pub name: String,
    pub is_default: bool,
    pub schema: DbSchemaState,
}

/// Lazy-fetch state machine for one `(conn, db)` schema slot. `Loading`
/// carries `prev_expanded` (resolved deviation 13): a ⟳ refresh of a Loaded
/// slot must carry its expand-set forward through the Loading transition
/// into `prune_stale_ids`, or the same-slot-refresh-preserves-expansion
/// contract (design §1.2) is silently lost.
pub enum DbSchemaState {
    NotLoaded,
    Loading { generation: u64, prev_expanded: HashSet<NodeId> },
    Error(String),
    Loaded { snapshot: SchemaSnapshot, expanded: HashSet<NodeId> },
}

/// One visible sidebar row: `(row, depth, label, is_expandable)` — the
/// multi-root analogue of `FlatNode`.
pub type SidebarFlatRow = (SidebarRow, usize, String, bool);

/// Design §6: bounded snapshot cache — at most this many `(conn, db)`
/// schema slots stay `Loaded` (LRU, never evicting the active slot). A
/// `SchemaSnapshot` can be thousands of objects; eight covers real cross-db
/// work while bounding memory on a hoarder server.
pub const LOADED_SNAPSHOT_CAP: usize = 8;

/// Display label for a Database row: file engines show the file stem —
/// the DATA MODEL keeps the full spec-level path (resolved deviation 5:
/// a name that can't round-trip into `spec_for_database` must not live
/// in `DbNode.name`).
pub fn display_db_name(engine: Engine, db: &str) -> String {
    if crate::connections_ui::engine_is_file_based(engine) {
        std::path::Path::new(db)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| db.to_string())
    } else {
        db.to_string()
    }
}

/// Design §5 row 9: a favourite belongs to the active scope when its
/// connection matches AND its database-or-default equals the scope's db.
pub fn favourite_in_scope(f: &FavouriteObject, scope: &ActiveScope) -> bool {
    f.connection_id == scope.conn_id
        && f.database.as_deref().unwrap_or(&scope.default_db) == scope.db
}

/// Design §5 row 1: whether a row may carry the ambient action icons
/// (★/⊞/⇪) and DDL-header enablement — `Inner` rows of the ACTIVE scope
/// and `Pinned` rows (active-context by definition) only. T5's render is
/// the sole consumer; pure so the gating is testable without GPUI.
pub fn row_in_active_scope(row: &SidebarRow, scope: Option<&ActiveScope>) -> bool {
    match row {
        SidebarRow::Pinned(_) => scope.is_some(),
        // CLI rows are the active context whenever no saved connection is
        // (the CLI root only renders in that state) — they keep the
        // ⊞/⇪/DDL affordances they had pre-rework; ★ still yields None via
        // favourite_object_for (no connection id to stamp).
        SidebarRow::Inner { conn_id, db, .. } => match scope {
            Some(s) => &s.conn_id == conn_id && &s.db == db,
            None => conn_id == crate::CLI_CONN_IDENTITY,
        },
        // Scripts rows are NEVER in scope and never favouritable — they
        // are files, not database objects (Part S §1.4).
        SidebarRow::Folder { .. }
        | SidebarRow::Connection { .. }
        | SidebarRow::Database { .. }
        | SidebarRow::Notice { .. }
        | SidebarRow::ScriptsRoot
        | SidebarRow::ScriptFolder { .. }
        | SidebarRow::ScriptFile { .. }
        | SidebarRow::ScriptNotice { .. } => false,
    }
}

/// Transitions a schema slot into `Loading`, carrying the previous
/// expand-set forward (from `Loaded` OR an in-flight `Loading` — a
/// superseding dispatch must not lose it either).
pub fn begin_schema_load(slot: &mut DbSchemaState, generation: u64) {
    let prev_expanded = match std::mem::replace(slot, DbSchemaState::NotLoaded) {
        DbSchemaState::Loaded { expanded, .. } => expanded,
        DbSchemaState::Loading { prev_expanded, .. } => prev_expanded,
        DbSchemaState::NotLoaded | DbSchemaState::Error(_) => HashSet::new(),
    };
    *slot = DbSchemaState::Loading { generation, prev_expanded };
}

/// Last-dispatched-wins: applies only while the slot is still Loading with
/// the SAME generation; anything else means a newer dispatch superseded
/// this result — drop it (design §1.2).
pub fn apply_schema_result(
    slot: &mut DbSchemaState,
    my_gen: u64,
    result: Result<SchemaSnapshot, String>,
) {
    let DbSchemaState::Loading { generation, prev_expanded } = slot else { return };
    if *generation != my_gen {
        return;
    }
    // Take ownership BEFORE reassigning *slot — building the new value
    // while still borrowing through `slot` would be an overlapping-borrow
    // compile error.
    let prev = std::mem::take(prev_expanded);
    *slot = match result {
        Err(e) => DbSchemaState::Error(e),
        Ok(snapshot) => {
            let (expanded, _sel) = prune_stale_ids(&prev, &None, &snapshot);
            DbSchemaState::Loaded { snapshot, expanded }
        }
    };
}

/// Transitions a connection's database list into `Loading`.
pub fn begin_db_list_load(node: &mut ConnNode, generation: u64) {
    // INVARIANT: only NotLoaded/Error re-dispatch (a Loaded list is cached;
    // re-expand is instant and the ⟳ refresh targets the active SCHEMA
    // slot, not the list). A begin over Loaded would drop every child
    // schema slot — refuse it here rather than trusting every caller.
    if matches!(node.dbs, DbListState::Loaded { .. }) {
        return;
    }
    node.dbs = DbListState::Loading { generation };
}

/// Last-dispatched-wins counterpart of `apply_schema_result` for the
/// database list; marks `is_default` against the saved config's database
/// and starts every schema slot at `NotLoaded` (lazy).
pub fn apply_db_list_result(
    node: &mut ConnNode,
    my_gen: u64,
    result: Result<(Vec<String>, bool), String>,
    default_db: &str,
) {
    let DbListState::Loading { generation } = &node.dbs else { return };
    if *generation != my_gen {
        return;
    }
    node.dbs = match result {
        Err(e) => DbListState::Error(e),
        Ok((names, truncated)) => DbListState::Loaded {
            dbs: names
                .into_iter()
                .map(|name| DbNode {
                    is_default: name == default_db,
                    schema: DbSchemaState::NotLoaded,
                    name,
                })
                .collect(),
            truncated,
        },
    };
}

/// T5 review MAJOR 1: the ACTIVE context's schema fallback slot, keyed by
/// its `(conn_id, db)` — exact `cli_slot` precedent. The switch path
/// (dropdown/palette/double-click) fetches the active schema BEFORE the
/// connection's database list was ever expanded; without this slot the
/// result had nowhere to land (`slot_mut` = None) and was silently dropped
/// — autocomplete/fk-joins/`detect_editable_pk`/palette/admin seed all
/// degraded until a manual double expand. NOT a synthesized one-entry
/// `Loaded` list: that would collide with `begin_db_list_load`'s
/// refuse-over-Loaded invariant.
pub type ActiveSlot = Option<((String, String), DbSchemaState)>;

/// Transitions the fallback slot into `Loading` for `(conn_id, db)`,
/// carrying its previous expand-set forward when the key matches (same
/// contract as `begin_schema_load`); a different key is replaced outright
/// (the fallback only ever serves ONE context at a time).
pub fn begin_fallback_schema_load(slot: &mut ActiveSlot, conn_id: &str, db: &str, generation: u64) {
    let mut state = match slot.take() {
        Some(((c, d), st)) if c == conn_id && d == db => st,
        _ => DbSchemaState::NotLoaded,
    };
    begin_schema_load(&mut state, generation);
    *slot = Some(((conn_id.to_string(), db.to_string()), state));
}

/// `apply_schema_result` for the fallback slot — applies ONLY when the key
/// matches `(conn_id, db)` (no cross-context leak: a fallback loaded for
/// one scope can never absorb — or answer for — another's result).
pub fn apply_fallback_schema_result(
    slot: &mut ActiveSlot,
    conn_id: &str,
    db: &str,
    my_gen: u64,
    result: Result<SchemaSnapshot, String>,
) {
    if let Some(((c, d), state)) = slot {
        if c == conn_id && d == db {
            apply_schema_result(state, my_gen, result);
        }
    }
}

/// Read access to the fallback slot, key-gated the same way.
pub fn fallback_slot<'a>(slot: &'a ActiveSlot, conn_id: &str, db: &str) -> Option<&'a DbSchemaState> {
    match slot {
        Some(((c, d), state)) if c == conn_id && d == db => Some(state),
        _ => None,
    }
}

/// Once `conn_id`'s database list loads, the fallback state migrates into
/// its real `DbNode` (the fallback was only ever a stand-in for the not-
/// yet-listed slot) and the fallback empties. Returns the migrated key
/// when a `Loaded` snapshot moved — the caller LRU-accounts it via
/// `touch_and_evict`. A list that does not contain the fallback's db
/// (e.g. truncated at the cap) keeps the fallback in place — the active
/// context must not lose its schema to a listing artifact.
pub fn migrate_fallback_into_list(
    node: &mut ConnNode,
    slot: &mut ActiveSlot,
    conn_id: &str,
) -> Option<(String, String)> {
    let Some(((c, _), _)) = slot else { return None };
    if c != conn_id {
        return None;
    }
    let DbListState::Loaded { dbs, .. } = &mut node.dbs else { return None };
    let db = slot.as_ref().map(|((_, d), _)| d.clone()).expect("checked above");
    let Some(dbn) = dbs.iter_mut().find(|x| x.name == db) else {
        return None;
    };
    let (key, state) = slot.take().expect("checked above");
    let was_loaded = matches!(state, DbSchemaState::Loaded { .. });
    dbn.schema = state;
    was_loaded.then_some(key)
}

/// Design §6: bounded snapshot cache. Push `touched` to the back of `lru`
/// (dedup), then evict `Loaded` slots beyond `LOADED_SNAPSHOT_CAP` back to
/// `NotLoaded`, oldest first, NEVER the active slot.
pub fn touch_and_evict(
    states: &mut HashMap<String, ConnNode>,
    lru: &mut Vec<(String, String)>,
    touched: (String, String),
    active: Option<&(String, String)>,
) {
    lru.retain(|k| k != &touched);
    lru.push(touched);
    let loaded: Vec<(String, String)> = lru
        .iter()
        .filter(|(c, d)| {
            states.get(c.as_str()).is_some_and(|n| match &n.dbs {
                DbListState::Loaded { dbs, .. } => dbs
                    .iter()
                    .any(|x| &x.name == d && matches!(x.schema, DbSchemaState::Loaded { .. })),
                _ => false,
            })
        })
        .cloned()
        .collect();
    if loaded.len() <= LOADED_SNAPSHOT_CAP {
        return;
    }
    let mut to_evict = loaded.len() - LOADED_SNAPSHOT_CAP;
    for key in loaded {
        if to_evict == 0 {
            break;
        }
        if Some(&key) == active {
            continue;
        }
        if let Some(node) = states.get_mut(&key.0) {
            if let DbListState::Loaded { dbs, .. } = &mut node.dbs {
                if let Some(d) = dbs.iter_mut().find(|d| d.name == key.1) {
                    d.schema = DbSchemaState::NotLoaded;
                    to_evict -= 1;
                }
            }
        }
        lru.retain(|k| k != &key || Some(k) == active);
    }
}

/// The multi-root analogue of `flatten` (design §1.1/§6): pure, GPUI-free,
/// called fresh every render by T5. A plain outer loop — NO recursion
/// anywhere (folder-path length is only an indent multiplier, never a
/// recursion depth); the inner tree reuses the existing iterative
/// `flatten_schema`.
///
/// Speed search filters LOADED content only (binding, design §6): this
/// function is pure over its inputs — typing can never trigger a fetch,
/// which holds by construction.
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

pub fn flatten_sidebar(
    grouped: &crate::connections_ui::GroupedConnections,
    states: &HashMap<String, ConnNode>,
    cli: Option<(&str, &DbSchemaState)>,
    outer_expanded: &HashSet<OuterId>,
    filter: &str,
    active: Option<&ActiveScope>,
    favourites: &[FavouriteObject],
    admin: AdminEntry,
    // Scripts library section (Part S §1.4/§3.3): `None` keeps the section
    // out of the sidebar entirely — the state Task 3 ships in, and the
    // state every pre-existing test asserts against. `Some((state,
    // configured))` = live (Task 7's flip); `configured` is
    // `AppView::effective_scripts_root().is_some()`.
    scripts: Option<(&ScriptsListState, bool)>,
) -> Vec<SidebarFlatRow> {
    let mut out: Vec<SidebarFlatRow> = Vec::new();
    let filter_lc = filter.to_lowercase();
    let filter_active = !filter_lc.is_empty();

    // Pinned rows — active context only, ONCE, above everything (design §1.1).
    if admin != AdminEntry::Hidden {
        out.push((SidebarRow::Pinned(NodeId::AdminRoot), 0, "Správa serveru".to_string(), false));
    }
    if let Some(scope) = active {
        let items: Vec<&FavouriteObject> =
            favourites.iter().filter(|f| favourite_in_scope(f, scope)).collect();
        if !items.is_empty() {
            out.push((
                SidebarRow::Pinned(NodeId::FavouriteSection),
                0,
                format!("Oblíbené ({})", items.len()),
                true,
            ));
            if outer_expanded.contains(&OuterId::Favourites) {
                for f in items {
                    let schema_key = f.schema.clone().unwrap_or_default();
                    let label = if schema_key.is_empty() {
                        f.name.clone()
                    } else {
                        format!("{}.{}", schema_key, f.name)
                    };
                    out.push((
                        SidebarRow::Pinned(NodeId::Favourite(f.kind.clone(), schema_key, f.name.clone())),
                        1,
                        label,
                        false,
                    ));
                }
            }
        }
    }

    // Scripts library (Part S §1.4): a third pinned root section, after
    // „Oblíbené" and before the CLI/connection roots. GLOBAL — unlike the
    // pinned rows above it, it does not depend on `active`.
    if let Some((state, configured)) = scripts {
        emit_scripts_section(&mut out, state, configured, outer_expanded, filter);
    }

    // CLI synthetic root (design §3.4 / resolved deviation 12): schema rows
    // splice directly under it with `db = ""` — no Database level, the CLI
    // session cannot switch databases.
    if let Some((label, slot)) = cli {
        let row = SidebarRow::Connection { conn_id: crate::CLI_CONN_IDENTITY.to_string() };
        out.push((row, 0, label.to_string(), true));
        if outer_expanded.contains(&OuterId::Connection(crate::CLI_CONN_IDENTITY.to_string())) {
            emit_schema_slot(&mut out, crate::CLI_CONN_IDENTITY, "", slot, 1, filter);
        }
    }

    // Saved connections: group_connections order verbatim (design §8) —
    // favourites first, then the BTreeMap-of-paths (loose before named
    // folders, parents before children, alphabetical within siblings).
    let emit_conn = |out: &mut Vec<SidebarFlatRow>, c: &ConnectionConfig, depth: usize| {
        let mut label = format!("{} ({})", c.name, crate::connections_ui::engine_label(c.engine));
        if c.read_only {
            label.push_str(" (pouze pro čtení)");
        }
        let conn_expanded = outer_expanded.contains(&OuterId::Connection(c.id.clone()));
        let start = out.len();
        out.push((SidebarRow::Connection { conn_id: c.id.clone() }, depth, label.clone(), true));
        if conn_expanded {
            match states.get(&c.id).map(|n| &n.dbs).unwrap_or(&DbListState::NotLoaded) {
                DbListState::NotLoaded => {} // expand handler is dispatching; nothing yet
                DbListState::Loading { .. } => out.push((
                    SidebarRow::Notice {
                        conn_id: c.id.clone(),
                        db: None,
                        text: "Načítám databáze…".into(),
                        retry: false,
                    },
                    depth + 1,
                    "Načítám databáze…".into(),
                    false,
                )),
                DbListState::Error(e) => out.push((
                    SidebarRow::Notice {
                        conn_id: c.id.clone(),
                        db: None,
                        text: format!("error: {e}"),
                        retry: true,
                    },
                    depth + 1,
                    format!("error: {e}"),
                    false,
                )),
                DbListState::Loaded { dbs, truncated } => {
                    for d in dbs {
                        let mut db_label = display_db_name(c.engine, &d.name);
                        if d.is_default {
                            db_label.push_str(" (výchozí)");
                        }
                        let db_start = out.len();
                        out.push((
                            SidebarRow::Database { conn_id: c.id.clone(), db: d.name.clone() },
                            depth + 1,
                            db_label.clone(),
                            true,
                        ));
                        if outer_expanded.contains(&OuterId::Database(c.id.clone(), d.name.clone())) {
                            emit_schema_slot(out, &c.id, &d.name, &d.schema, depth + 2, filter);
                        }
                        // Filter: drop a childless db row whose own label misses.
                        if filter_active
                            && out.len() == db_start + 1
                            && !name_matches(&db_label, &filter_lc)
                        {
                            out.truncate(db_start);
                        }
                    }
                    if *truncated {
                        let text =
                            "… zobrazeno prvních 2000 databází — použijte výchozí databázi nebo filtr"
                                .to_string();
                        out.push((
                            SidebarRow::Notice {
                                conn_id: c.id.clone(),
                                db: None,
                                text: text.clone(),
                                retry: false,
                            },
                            depth + 1,
                            text,
                            false,
                        ));
                    }
                }
            }
        }
        // Filter: drop a childless connection row whose own label misses.
        if filter_active && out.len() == start + 1 && !name_matches(&label, &filter_lc) {
            out.truncate(start);
        }
    };

    for c in &grouped.favourites {
        emit_conn(&mut out, c, 0);
    }
    for group in &grouped.folders {
        if group.path.is_empty() {
            for c in &group.connections {
                emit_conn(&mut out, c, 0);
            }
            continue;
        }
        let depth = group.path.len() - 1;
        let folder_start = out.len();
        out.push((
            SidebarRow::Folder { path: group.path.clone() },
            depth,
            group.path.last().cloned().unwrap_or_default(),
            true,
        ));
        // A folder is "collapsed" when its OuterId is IN the set — see the
        // polarity note on `OuterId` (folders default to expanded).
        let collapsed = outer_expanded.contains(&OuterId::Folder(group.path.clone()));
        if !collapsed {
            for c in &group.connections {
                emit_conn(&mut out, c, group.path.len());
            }
        }
        if filter_active && out.len() == folder_start + 1 {
            out.truncate(folder_start); // childless folder under a filter
        }
    }
    out
}

/// Splices one `(conn, db)` slot's rows: Notice for Loading/Error, the
/// EXISTING `flatten_schema` output (depth-shifted, wrapped in
/// `SidebarRow::Inner`) for Loaded. Pure; never fetches. Takes the RAW
/// filter — `flatten_schema` lowercases internally (it inherited the old
/// `flatten` body verbatim, which did); `flatten_sidebar`'s own label
/// matching uses its locally-lowercased copy. ONE convention per layer,
/// pinned by `filter_narrows_loaded_content_and_matches_row_labels`.
fn emit_schema_slot(
    out: &mut Vec<SidebarFlatRow>,
    conn_id: &str,
    db: &str,
    slot: &DbSchemaState,
    base_depth: usize,
    filter: &str,
) {
    match slot {
        DbSchemaState::NotLoaded => {}
        DbSchemaState::Loading { .. } => out.push((
            SidebarRow::Notice {
                conn_id: conn_id.to_string(),
                db: Some(db.to_string()),
                text: "Načítám schéma…".into(),
                retry: false,
            },
            base_depth,
            "Načítám schéma…".into(),
            false,
        )),
        DbSchemaState::Error(e) => out.push((
            SidebarRow::Notice {
                conn_id: conn_id.to_string(),
                db: Some(db.to_string()),
                text: format!("error: {e}"),
                retry: true,
            },
            base_depth,
            format!("error: {e}"),
            false,
        )),
        DbSchemaState::Loaded { snapshot, expanded } => {
            for (node, depth, label, expandable) in flatten_schema(snapshot, expanded, filter) {
                out.push((
                    SidebarRow::Inner { conn_id: conn_id.to_string(), db: db.to_string(), node },
                    base_depth + depth,
                    label,
                    expandable,
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------
// GPUI entity.
// ---------------------------------------------------------------------

actions!(schema_tree, [TreeEscape]);

/// Binds the tree's own "escape clears the filter, then blurs to the
/// editor" action, scoped to context "SchemaTree" so it takes priority over
/// the app-level unscoped `escape` -> `CancelQuery` binding (GPUI prefers a
/// context-scoped binding over an unscoped one when both match — same
/// precedent as `TextField`'s scoped `backspace` in connections_ui.rs
/// winning over `SqlInput`'s unscoped one). Esc semantics (brief contract
/// #5, "Esc clears filter first, then collapses focus"): filter non-empty
/// -> clear it; filter already empty -> blur the tree to the SQL editor
/// (`on_tree_escape`). Once focus has moved off the tree, `SchemaTree`'s
/// scoped binding no longer applies, so a further Esc hits the app-level
/// unscoped `CancelQuery` binding directly and cancels a running query as
/// normal.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", TreeEscape, Some("SchemaTree"))]);
}

pub struct SchemaTree {
    /// Folder/favourite grouping of the saved connections — pushed by
    /// `main.rs` (`sync_connections`) on startup and after every config
    /// mutation; the tree never owns a second long-term copy of the config.
    grouped: crate::connections_ui::GroupedConnections,
    /// Per-connection lazy sidebar state, keyed by connection id. An id
    /// missing from the map renders as `NotLoaded` (see `flatten_sidebar`),
    /// but `sync_connections` keeps an entry per saved connection so
    /// `begin_db_list` always has a slot to write into.
    conns: HashMap<String, ConnNode>,
    /// LRU order of `(conn_id, db)` schema-slot touches — `touch_and_evict`
    /// bounds the number of `Loaded` snapshots at `LOADED_SNAPSHOT_CAP`.
    lru: Vec<(String, String)>,
    /// Expand state for the OUTER (multi-root) levels — see `OuterId`'s
    /// polarity note (folders inverted: presence = collapsed).
    outer_expanded: HashSet<OuterId>,
    /// The active `(connection, database)` context, pushed by `main.rs`
    /// (`set_active_scope` via `push_active_scope_to_tree`). `None` = CLI
    /// context or no connection at all.
    active_scope: Option<ActiveScope>,
    /// The CLI-arg URL (design §3.4): renders a synthetic root whose schema
    /// rows splice directly under it (no Database level — the CLI session
    /// cannot switch databases). `None` once a saved connection has been
    /// switched to (the CLI root disappears, and cannot come back).
    cli_url: Option<String>,
    /// The CLI root's schema slot (`conn_id == CLI_CONN_IDENTITY`,
    /// `db == ""` in every `(conn, db)` API here).
    cli_slot: DbSchemaState,
    /// T5 review MAJOR 1: the ACTIVE context's schema fallback — holds the
    /// switch path's schema fetch when the connection's database list was
    /// never expanded (no `DbNode` slot exists yet). Consulted by
    /// `begin_schema`/`finish_schema`/`slot_ref` when the map slot is
    /// absent; migrated into the real `DbNode` once the list loads
    /// (`finish_db_list`). See `ActiveSlot`'s doc comment.
    active_slot: ActiveSlot,
    filter: String,
    selected: Option<SidebarRow>,
    focus_handle: FocusHandle,
    /// The SQL editor's focus handle, handed in by `main.rs` at construction
    /// (it owns both entities) so `on_tree_escape` can blur the tree back to
    /// the editor directly, without needing a `TreeEvent` round-trip through
    /// `AppView` (which doesn't have `Window` access in its `cx.subscribe`
    /// callback) — see `on_tree_escape`.
    editor_focus: FocusHandle,
    /// G3 Task 4: `AppConfig::favourite_objects`, pushed in by `main.rs`
    /// (`set_favourites`) on every tree-context refresh and after every ★
    /// toggle — NOT filtered by connection here; `flatten_sidebar`/
    /// `favourite_object_for` filter against `active_scope` (sidebar
    /// rework: the old `active_connection_id` parameter is gone — the
    /// scope subsumes it).
    favourites: Vec<FavouriteObject>,
    /// G12 T4: `AppView::active_read_only()`, pushed in alongside every
    /// snapshot/favourites update (`main.rs::refresh_tree_context`) — gates
    /// the per-table-row "⇪" CSV-import affordance (CURATION item 4(b)'s
    /// entry-gate half: hidden entirely, not merely disabled, on a
    /// read-only connection).
    read_only: bool,
    /// G10 T4: `admin_panel::admin_entry_state`'s result for the ACTIVE
    /// connection, pushed in by `main.rs` everywhere `set_favourites` is
    /// already called on a connection switch/schema refresh (`set_admin_entry`).
    /// Drives both `flatten`'s pinned "Správa serveru" row visibility and
    /// this row's greyed/disabled rendering.
    admin_entry: AdminEntry,
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
}

impl SchemaTree {
    pub fn new(cx: &mut Context<Self>, editor_focus: FocusHandle) -> Self {
        Self {
            grouped: crate::connections_ui::GroupedConnections::default(),
            conns: HashMap::new(),
            lru: Vec::new(),
            outer_expanded: HashSet::new(),
            active_scope: None,
            cli_url: None,
            cli_slot: DbSchemaState::NotLoaded,
            active_slot: None,
            filter: String::new(),
            selected: None,
            focus_handle: cx.focus_handle(),
            editor_focus,
            favourites: Vec::new(),
            read_only: false,
            admin_entry: AdminEntry::Hidden,
            scripts: ScriptsListState::NotLoaded,
            scripts_configured: false,
            scripts_generation: 0,
        }
    }

    /// Sidebar rework: re-sync the saved-connection roots after a config
    /// mutation (add/rename/delete/favourite/folder move). Keeps existing
    /// per-connection state for still-present ids, seeds `NotLoaded` for
    /// new ids, and drops removed ids (with their LRU entries).
    pub fn sync_connections(
        &mut self,
        grouped: crate::connections_ui::GroupedConnections,
        cx: &mut Context<Self>,
    ) {
        let ids: HashSet<String> = grouped
            .favourites
            .iter()
            .chain(grouped.folders.iter().flat_map(|g| g.connections.iter()))
            .map(|c| c.id.clone())
            .collect();
        self.conns.retain(|id, _| ids.contains(id));
        for id in &ids {
            self.conns
                .entry(id.clone())
                .or_insert_with(|| ConnNode { dbs: DbListState::NotLoaded });
        }
        self.lru.retain(|(c, _)| ids.contains(c));
        self.grouped = grouped;
        cx.notify();
    }

    /// Sidebar rework: the active `(connection, database)` context — drives
    /// the ● indicator, icon gating, favourites filtering and `snapshot()`.
    pub fn set_active_scope(&mut self, scope: Option<ActiveScope>, cx: &mut Context<Self>) {
        self.active_scope = scope;
        cx.notify();
    }

    /// Sidebar rework (design §3.4): the CLI synthetic root's URL. A switch
    /// to a saved connection sets it `None` — the CLI root disappears for
    /// good (its slot is dropped too; `conn_url` is never re-set).
    pub fn set_cli(&mut self, url: Option<String>, cx: &mut Context<Self>) {
        if url.is_none() {
            self.cli_slot = DbSchemaState::NotLoaded;
        }
        self.cli_url = url;
        cx.notify();
    }

    /// Transitions `conn_id`'s database list into `Loading` under
    /// `generation` (no-op over a `Loaded` list — see `begin_db_list_load`).
    pub fn begin_db_list(&mut self, conn_id: &str, generation: u64, cx: &mut Context<Self>) {
        if let Some(node) = self.conns.get_mut(conn_id) {
            begin_db_list_load(node, generation);
            cx.notify();
        }
    }

    /// Applies a database-list result (stale generations dropped by
    /// `apply_db_list_result` — last-dispatched wins), then migrates any
    /// active-context fallback schema into its real `DbNode` (T5 review
    /// MAJOR 1) — LRU-accounting a migrated `Loaded` snapshot.
    pub fn finish_db_list(
        &mut self,
        conn_id: &str,
        generation: u64,
        result: Result<(Vec<String>, bool), String>,
        default_db: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.conns.get_mut(conn_id) else { return };
        apply_db_list_result(node, generation, result, default_db);
        if let Some(key) = migrate_fallback_into_list(node, &mut self.active_slot, conn_id) {
            let active = self.active_scope.as_ref().map(|s| (s.conn_id.clone(), s.db.clone()));
            touch_and_evict(&mut self.conns, &mut self.lru, key, active.as_ref());
        }
        cx.notify();
    }

    /// Transitions one `(conn, db)` schema slot into `Loading`
    /// (`CLI_CONN_IDENTITY` targets the CLI slot; `db` is ignored there).
    /// A missing map slot for the ACTIVE scope lands in the fallback
    /// `active_slot` instead of being dropped (T5 review MAJOR 1 — the
    /// switch path fetches before the db list exists); non-active scopes
    /// without a slot still no-op (their result is generation-dropped).
    pub fn begin_schema(&mut self, conn_id: &str, db: &str, generation: u64, cx: &mut Context<Self>) {
        if conn_id == crate::CLI_CONN_IDENTITY {
            begin_schema_load(&mut self.cli_slot, generation);
        } else if let Some(slot) = self.slot_mut(conn_id, db) {
            begin_schema_load(slot, generation);
        } else if self
            .active_scope
            .as_ref()
            .is_some_and(|s| s.conn_id == conn_id && s.db == db)
        {
            begin_fallback_schema_load(&mut self.active_slot, conn_id, db, generation);
        } else {
            // No such slot (list not loaded / db vanished) and not the
            // active context — the fetch result will be dropped by the
            // generation check anyway.
            return;
        }
        cx.notify();
    }

    /// Applies a schema-fetch result to its slot (stale generations dropped
    /// by `apply_schema_result`) and drives the LRU cap — the ACTIVE slot
    /// is never evicted (`touch_and_evict`).
    pub fn finish_schema(
        &mut self,
        conn_id: &str,
        db: &str,
        generation: u64,
        result: Result<SchemaSnapshot, String>,
        cx: &mut Context<Self>,
    ) {
        if conn_id == crate::CLI_CONN_IDENTITY {
            apply_schema_result(&mut self.cli_slot, generation, result);
            cx.notify();
            return;
        }
        let Some(slot) = self.slot_mut(conn_id, db) else {
            // T5 review MAJOR 1: the active-context fallback catches the
            // switch path's result when the db list was never loaded —
            // key-gated, so no cross-context result can land here.
            apply_fallback_schema_result(&mut self.active_slot, conn_id, db, generation, result);
            cx.notify();
            return;
        };
        apply_schema_result(slot, generation, result);
        let active = self.active_scope.as_ref().map(|s| (s.conn_id.clone(), s.db.clone()));
        touch_and_evict(
            &mut self.conns,
            &mut self.lru,
            (conn_id.to_string(), db.to_string()),
            active.as_ref(),
        );
        cx.notify();
    }

    /// Vault-prompt cancel path (design §1.3): the user declined — collapse
    /// the row back; its state stays `NotLoaded`, no error row.
    pub fn collapse_connection(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        self.outer_expanded.remove(&OuterId::Connection(conn_id.to_string()));
        cx.notify();
    }

    /// Mutable access to one `(conn, db)` schema slot (saved connections
    /// only — the CLI slot is special-cased by callers).
    fn slot_mut(&mut self, conn_id: &str, db: &str) -> Option<&mut DbSchemaState> {
        let DbListState::Loaded { dbs, .. } = &mut self.conns.get_mut(conn_id)?.dbs else {
            return None;
        };
        dbs.iter_mut().find(|d| d.name == db).map(|d| &mut d.schema)
    }

    /// Immutable sibling of `slot_mut` (CLI slot for the
    /// `CLI_CONN_IDENTITY` id, any `db`). Falls back to the key-gated
    /// `active_slot` when the map slot is absent (T5 review MAJOR 1).
    fn slot_ref(&self, conn_id: &str, db: &str) -> Option<&DbSchemaState> {
        if conn_id == crate::CLI_CONN_IDENTITY {
            return Some(&self.cli_slot);
        }
        if let Some(node) = self.conns.get(conn_id) {
            if let DbListState::Loaded { dbs, .. } = &node.dbs {
                if let Some(d) = dbs.iter().find(|d| d.name == db) {
                    return Some(&d.schema);
                }
            }
        }
        fallback_slot(&self.active_slot, conn_id, db)
    }

    /// One `(conn, db)` slot's snapshot, if `Loaded`.
    fn snapshot_for(&self, conn_id: &str, db: &str) -> Option<&SchemaSnapshot> {
        match self.slot_ref(conn_id, db)? {
            DbSchemaState::Loaded { snapshot, .. } => Some(snapshot),
            _ => None,
        }
    }

    /// Sidebar rework (T6's compare picker): the fetched database list for
    /// `conn_id`, with its truncation flag, if `Loaded` — consumed by
    /// `render_modal_overlay`'s CompareDialog arm (cached lists only, the
    /// dialog never triggers fetches).
    pub fn db_list_for(&self, conn_id: &str) -> Option<(&[DbNode], bool)> {
        match &self.conns.get(conn_id)?.dbs {
            DbListState::Loaded { dbs, truncated } => Some((dbs.as_slice(), *truncated)),
            _ => None,
        }
    }

    /// G12 T4: see the `read_only` field's doc comment.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = read_only;
        cx.notify();
    }

    /// G10 T4: see the `admin_entry` field's doc comment.
    pub fn set_admin_entry(&mut self, admin_entry: AdminEntry, cx: &mut Context<Self>) {
        self.admin_entry = admin_entry;
        cx.notify();
    }

    /// Called by `main.rs` on every tree-context refresh
    /// (`refresh_tree_context`) and again right after a ★ toggle resolves
    /// (`config.toggle_favourite` + guarded save) — the "Oblíbené" section
    /// and every row's star state are recomputed fresh from this field on
    /// the very next render. Sidebar rework: the old `active_connection_id`
    /// parameter is gone — filtering runs against `active_scope`
    /// (`favourite_in_scope`, design §5 row 9).
    pub fn set_favourites(&mut self, favourites: Vec<FavouriteObject>, cx: &mut Context<Self>) {
        self.favourites = favourites;
        cx.notify();
    }

    /// The `FavouriteObject` a given row's ★/☆ toggle targets, or `None`
    /// for rows that don't support favouriting and for every row OUTSIDE
    /// the active scope (design §5 row 1: cross-context ambient actions
    /// don't exist; CLI rows have no connection id to stamp — `None` too,
    /// pre-rework posture). For `NodeId::Table`, the table/view distinction
    /// is looked up from the ACTIVE slot's snapshot; a table that's since
    /// vanished (a rename/drop raced with the click) safely yields `None`.
    /// Stamps `database`: `Some(db)` for a non-default active db, `None`
    /// for the default (design §5 row 9's back-compat rule — existing
    /// favourites keep meaning what they meant).
    fn favourite_object_for(&self, row: &SidebarRow) -> Option<FavouriteObject> {
        let scope = self.active_scope.as_ref()?;
        let node = match row {
            SidebarRow::Inner { conn_id, db, node }
                if conn_id == &scope.conn_id && db == &scope.db =>
            {
                node
            }
            SidebarRow::Pinned(node @ NodeId::Favourite(..)) => node,
            _ => return None,
        };
        let database = (scope.db != scope.default_db).then(|| scope.db.clone());
        let connection_id = scope.conn_id.clone();
        let schema_opt = |s: &str| if s.is_empty() { None } else { Some(s.to_string()) };
        match node {
            NodeId::Table(schema, name) => {
                let kind = self
                    .snapshot()?
                    .tables
                    .iter()
                    .find(|t| &t.name == name && &schema_key_string(&t.schema) == schema)
                    .map(|t| match t.kind {
                        TableKind::Table => "table",
                        TableKind::View | TableKind::MaterializedView => "view",
                    })?;
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: kind.to_string(), database })
            }
            NodeId::Routine(schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: "routine".into(), database })
            }
            NodeId::Trigger(schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: "trigger".into(), database })
            }
            NodeId::Sequence(schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: "sequence".into(), database })
            }
            NodeId::Favourite(kind, schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: kind.clone(), database })
            }
            _ => None,
        }
    }

    /// The ACTIVE context's snapshot — same signature as the single-root
    /// era so every `main.rs` consumer (fk lookups, editable detection,
    /// palette items, autocomplete, admin schema seed) compiles untouched.
    /// The CLI slot answers when no saved connection is active and a CLI
    /// URL exists.
    pub fn snapshot(&self) -> Option<&SchemaSnapshot> {
        if let Some(scope) = &self.active_scope {
            return self.snapshot_for(&scope.conn_id, &scope.db);
        }
        if self.cli_url.is_some() {
            if let DbSchemaState::Loaded { snapshot, .. } = &self.cli_slot {
                return Some(snapshot);
            }
        }
        None
    }

    /// Chevron/double-click on an Inner row: toggles the `NodeId` in ITS
    /// slot's expand set (kept per-slot so collapsing a database and
    /// re-expanding it restores the inner shape — design §1.2).
    fn toggle_inner(&mut self, conn_id: &str, db: &str, node: &NodeId) {
        let slot = if conn_id == crate::CLI_CONN_IDENTITY {
            Some(&mut self.cli_slot)
        } else {
            self.slot_mut(conn_id, db)
        };
        if let Some(DbSchemaState::Loaded { expanded, .. }) = slot {
            if !expanded.remove(node) {
                expanded.insert(node.clone());
            }
        }
    }

    /// Chevron on a Folder/Connection/Database/FavouriteSection row.
    /// Folders are INVERTED (presence in the set = collapsed; they default
    /// open — see `OuterId`'s doc comment); everything else presence = open.
    fn toggle_outer(&mut self, row: &SidebarRow) {
        let id = match row {
            SidebarRow::Folder { path } => OuterId::Folder(path.clone()),
            SidebarRow::Connection { conn_id } => OuterId::Connection(conn_id.clone()),
            SidebarRow::Database { conn_id, db } => OuterId::Database(conn_id.clone(), db.clone()),
            SidebarRow::Pinned(NodeId::FavouriteSection) => OuterId::Favourites,
            SidebarRow::ScriptsRoot => OuterId::Scripts,
            SidebarRow::ScriptFolder { rel } => OuterId::ScriptFolder(rel.clone()),
            _ => return,
        };
        if !self.outer_expanded.remove(&id) {
            self.outer_expanded.insert(id);
        }
    }

    fn find_routine_ddl_in(&self, conn_id: &str, db: &str, schema: &str, name: &str) -> Option<String> {
        self.snapshot_for(conn_id, db)?
            .routines
            .iter()
            .find(|r| r.name == name && schema_key_string(&r.schema) == schema)
            .and_then(|r| r.ddl.clone())
    }

    fn find_trigger_ddl_in(&self, conn_id: &str, db: &str, schema: &str, name: &str) -> Option<String> {
        self.snapshot_for(conn_id, db)?
            .triggers
            .iter()
            .find(|t| t.name == name && schema_key_string(&t.schema) == schema)
            .and_then(|t| t.ddl.clone())
    }

    /// The currently-selected table/view's `TableInfo`, if `selected` is an
    /// `Inner` Table row AT THE ACTIVE SCOPE (design §5 row 1: the DDL
    /// header button is an ambient action — cross-context rows don't arm
    /// it) — looked up in `snapshot()`, the active slot.
    fn selected_table(&self) -> Option<&TableInfo> {
        let Some(SidebarRow::Inner { conn_id, db, node: NodeId::Table(schema, name) }) =
            self.selected.as_ref()
        else {
            return None;
        };
        if !row_in_active_scope(
            &SidebarRow::Inner {
                conn_id: conn_id.clone(),
                db: db.clone(),
                node: NodeId::Table(schema.clone(), name.clone()),
            },
            self.active_scope.as_ref(),
        ) {
            return None;
        }
        self.snapshot()?
            .tables
            .iter()
            .find(|t| &t.name == name && &schema_key_string(&t.schema) == schema)
    }

    /// Task 7 "Generate DDL" affordance (brief contract #3): the tree
    /// header's "DDL" button, enabled whenever a table/view is selected.
    /// No DB round-trip — uses the table's own `ddl` if the driver captured
    /// one (Postgres `pg_get_viewdef`/similar), else synthesizes a
    /// `CREATE TABLE` from the snapshot's column/constraint metadata
    /// (`dbc_core::ddl::synthesize_create_table`). Emits the same
    /// `TreeEvent::OpenDdl` double-click on a routine/trigger already uses,
    /// so `main.rs` needs no separate handling.
    fn handle_generate_ddl(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.selected_table() else { return };
        let ddl = t.ddl.clone().unwrap_or_else(|| synthesize_create_table(t));
        cx.emit(TreeEvent::OpenDdl { title: t.name.clone(), ddl });
    }

    /// Contract #4, widened over `SidebarRow` (sidebar rework): double-click
    /// table/view -> `OpenPreview` (scope-stamped); routine/trigger ->
    /// `OpenDdl`; Database/Connection rows -> `SwitchToDatabase` (design
    /// §2.1 — a double-click on the ALREADY-active row is a no-op inside
    /// `switch_to_database`'s identity check); otherwise toggle expand.
    fn handle_double_click(&mut self, row: &SidebarRow, cx: &mut Context<Self>) {
        self.selected = Some(row.clone());
        match row {
            // Design §2.1: double-click on a Database row switches; on a
            // Connection row switches to the DEFAULT db (dropdown parity).
            // Expanding (chevron) never switches — browsing ≠ switching.
            SidebarRow::Database { conn_id, db } => cx.emit(TreeEvent::SwitchToDatabase {
                conn_id: conn_id.clone(),
                db: Some(db.clone()),
            }),
            SidebarRow::Connection { conn_id } if conn_id != crate::CLI_CONN_IDENTITY => {
                cx.emit(TreeEvent::SwitchToDatabase { conn_id: conn_id.clone(), db: None })
            }
            // The CLI root cannot switch (design §3.4) — double-click just
            // toggles it, same as a folder.
            SidebarRow::Connection { .. } | SidebarRow::Folder { .. } => self.toggle_outer(row),
            SidebarRow::Inner { conn_id, db, node } => match node {
                NodeId::Table(schema, name) => {
                    let schema = if schema.is_empty() { None } else { Some(schema.clone()) };
                    cx.emit(TreeEvent::OpenPreview {
                        conn_id: conn_id.clone(),
                        db: db.clone(),
                        schema,
                        table: name.clone(),
                    });
                }
                NodeId::Routine(schema, name) => {
                    let ddl = self
                        .find_routine_ddl_in(conn_id, db, schema, name)
                        .unwrap_or_else(|| DDL_FALLBACK.to_string());
                    cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
                }
                NodeId::Trigger(schema, name) => {
                    let ddl = self
                        .find_trigger_ddl_in(conn_id, db, schema, name)
                        .unwrap_or_else(|| DDL_FALLBACK.to_string());
                    cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
                }
                // Everything else (Schema/Section/Column/Index/…): toggle
                // the row's SLOT-LOCAL inner expand set.
                other => {
                    let (conn_id, db, other) = (conn_id.clone(), db.clone(), other.clone());
                    self.toggle_inner(&conn_id, &db, &other);
                }
            },
            // Pinned favourite rows keep the pre-flip semantics verbatim,
            // resolved against the ACTIVE slot's snapshot: table/view →
            // OpenPreview (with the active scope's conn/db), routine/
            // trigger → OpenDdl, sequence → no-op; the section header
            // toggles `OuterId::Favourites`; AdminRoot double-click is a
            // no-op (single click handles it, as today).
            SidebarRow::Pinned(node) => {
                if let (NodeId::Favourite(kind, schema, name), Some(scope)) =
                    (node, self.active_scope.clone())
                {
                    let schema_opt = if schema.is_empty() { None } else { Some(schema.clone()) };
                    match kind.as_str() {
                        "table" | "view" => cx.emit(TreeEvent::OpenPreview {
                            conn_id: scope.conn_id,
                            db: scope.db,
                            schema: schema_opt,
                            table: name.clone(),
                        }),
                        "routine" => {
                            let ddl = self
                                .find_routine_ddl_in(&scope.conn_id, &scope.db, schema, name)
                                .unwrap_or_else(|| DDL_FALLBACK.to_string());
                            cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
                        }
                        "trigger" => {
                            let ddl = self
                                .find_trigger_ddl_in(&scope.conn_id, &scope.db, schema, name)
                                .unwrap_or_else(|| DDL_FALLBACK.to_string());
                            cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
                        }
                        _ => {}
                    }
                } else if matches!(node, NodeId::FavouriteSection) {
                    if !self.outer_expanded.remove(&OuterId::Favourites) {
                        self.outer_expanded.insert(OuterId::Favourites);
                    }
                }
            }
            SidebarRow::Notice { .. } => {}
            // Scripts rows: the root and folders toggle, exactly like a
            // grouping folder or the CLI root. A ScriptFile's "open into the
            // editor" needs `TreeEvent::ScriptOpen`, which lands with its
            // `main.rs` handler (Task 7/8) — inert here.
            SidebarRow::ScriptsRoot | SidebarRow::ScriptFolder { .. } => self.toggle_outer(row),
            SidebarRow::ScriptFile { .. } | SidebarRow::ScriptNotice { .. } => {}
        }
        cx.notify();
    }

    /// Chevron click. Expanding a Connection whose db list is
    /// `NotLoaded`/`Error` ALSO emits `LoadDatabases`; expanding a Database
    /// whose schema slot is `NotLoaded`/`Error` ALSO emits `LoadSchema`
    /// (lazy, design §1.2 — collapsing/re-expanding a cached slot fetches
    /// nothing). The CLI root's slot re-fetch rides `LoadSchema` with
    /// `db == ""`.
    fn handle_chevron(&mut self, row: &SidebarRow, cx: &mut Context<Self>) {
        match row {
            SidebarRow::Folder { .. } | SidebarRow::Pinned(NodeId::FavouriteSection) => {
                self.toggle_outer(row)
            }
            SidebarRow::Connection { conn_id } => {
                let was_expanded =
                    self.outer_expanded.contains(&OuterId::Connection(conn_id.clone()));
                self.toggle_outer(row);
                if !was_expanded {
                    if conn_id == crate::CLI_CONN_IDENTITY {
                        if matches!(self.cli_slot, DbSchemaState::NotLoaded | DbSchemaState::Error(_)) {
                            cx.emit(TreeEvent::LoadSchema {
                                conn_id: conn_id.clone(),
                                db: String::new(),
                            });
                        }
                    } else if matches!(
                        self.conns.get(conn_id).map(|n| &n.dbs),
                        None | Some(DbListState::NotLoaded) | Some(DbListState::Error(_))
                    ) {
                        cx.emit(TreeEvent::LoadDatabases { conn_id: conn_id.clone() });
                    }
                }
            }
            SidebarRow::Database { conn_id, db } => {
                let was_expanded = self
                    .outer_expanded
                    .contains(&OuterId::Database(conn_id.clone(), db.clone()));
                self.toggle_outer(row);
                if !was_expanded
                    && matches!(
                        self.slot_ref(conn_id, db),
                        None | Some(DbSchemaState::NotLoaded) | Some(DbSchemaState::Error(_))
                    )
                {
                    cx.emit(TreeEvent::LoadSchema { conn_id: conn_id.clone(), db: db.clone() });
                }
            }
            SidebarRow::Inner { conn_id, db, node } => {
                let (conn_id, db, node) = (conn_id.clone(), db.clone(), node.clone());
                self.toggle_inner(&conn_id, &db, &node);
            }
            SidebarRow::Pinned(_) | SidebarRow::Notice { .. } => {}
            // Task 3 only TOGGLES. The scan dispatch (emitting
            // `TreeEvent::ScriptsRefresh` when the slot is
            // `NotLoaded`/`Error`) lands in Task 7 together with `main.rs`'s
            // handler, because `TreeEvent`'s match in `AppView::on_tree_event`
            // is exhaustive and `main.rs` is not this task's file.
            SidebarRow::ScriptsRoot | SidebarRow::ScriptFolder { .. } => self.toggle_outer(row),
            SidebarRow::ScriptFile { .. } | SidebarRow::ScriptNotice { .. } => {}
        }
        cx.notify();
    }

    /// Single click: select. `Notice { retry: true }` rows instead RE-EMIT
    /// their Load event (`db: None` → `LoadDatabases`, `Some` →
    /// `LoadSchema`); the pinned AdminRoot keeps its pre-rework "click
    /// opens when Enabled, never selects" semantics.
    fn handle_single_click(&mut self, row: &SidebarRow, cx: &mut Context<Self>) {
        match row {
            SidebarRow::Notice { conn_id, db, retry: true, .. } => match db {
                None => cx.emit(TreeEvent::LoadDatabases { conn_id: conn_id.clone() }),
                Some(db) => cx.emit(TreeEvent::LoadSchema {
                    conn_id: conn_id.clone(),
                    db: db.clone(),
                }),
            },
            SidebarRow::Notice { .. } => {}
            // Inert here: the notice's retry / open-„Nastavení" click needs
            // `TreeEvent::ScriptsRefresh`/`OpenScriptsSettings`, which land
            // with their `main.rs` handlers in Task 7.
            SidebarRow::ScriptNotice { .. } => {}
            SidebarRow::Pinned(NodeId::AdminRoot) => {
                if self.admin_entry == AdminEntry::Enabled {
                    cx.emit(TreeEvent::OpenAdmin);
                }
            }
            _ => {
                self.selected = Some(row.clone());
            }
        }
        cx.notify();
    }

    /// The ▾/▸ chevron state for one row — outer rows read `outer_expanded`
    /// (folders inverted), Inner rows read their slot's expand set (with
    /// the same filter-active auto-expand `flatten_schema` applies).
    fn row_is_expanded(&self, row: &SidebarRow) -> bool {
        match row {
            SidebarRow::Folder { path } => {
                !self.outer_expanded.contains(&OuterId::Folder(path.clone()))
            }
            SidebarRow::Connection { conn_id } => {
                self.outer_expanded.contains(&OuterId::Connection(conn_id.clone()))
            }
            SidebarRow::Database { conn_id, db } => self
                .outer_expanded
                .contains(&OuterId::Database(conn_id.clone(), db.clone())),
            SidebarRow::Pinned(NodeId::FavouriteSection) => {
                self.outer_expanded.contains(&OuterId::Favourites)
            }
            SidebarRow::Inner { conn_id, db, node } => {
                if !self.filter.is_empty() {
                    return true; // filter auto-expands (mirrors `is_expanded`)
                }
                match self.slot_ref(conn_id, db) {
                    Some(DbSchemaState::Loaded { expanded, .. }) => expanded.contains(node),
                    _ => false,
                }
            }
            SidebarRow::ScriptsRoot => self.outer_expanded.contains(&OuterId::Scripts),
            SidebarRow::ScriptFolder { rel } => {
                // Filter auto-expands, mirroring `emit_scripts_section`.
                !self.filter.is_empty()
                    || self.outer_expanded.contains(&OuterId::ScriptFolder(rel.clone()))
            }
            SidebarRow::ScriptFile { .. } | SidebarRow::ScriptNotice { .. } => false,
            SidebarRow::Pinned(_) | SidebarRow::Notice { .. } => false,
        }
    }

    /// Esc: clears an active filter and consumes the keystroke; with an
    /// empty filter, blurs the tree by moving focus to the SQL editor
    /// (`editor_focus`, handed in by `main.rs` at construction — see the
    /// struct doc comment) instead of propagating. This matches the brief's
    /// "clears filter first, then collapses focus" contract; a further Esc
    /// after the blur reaches the app-level `CancelQuery` binding normally
    /// since the tree (and its scoped binding) no longer has focus.
    fn on_tree_escape(&mut self, _: &TreeEscape, window: &mut Window, cx: &mut Context<Self>) {
        if !self.filter.is_empty() {
            self.filter.clear();
            cx.notify();
        } else {
            window.focus(&self.editor_focus, cx);
            cx.notify();
        }
    }

    /// Speed search (contract #5/#7): printable characters append to
    /// `filter`, Backspace removes the last one. Bound as a raw
    /// `on_key_down` listener (not an action) precisely so it only reacts
    /// to keys with no matching binding — Ctrl/Cmd/Alt-held combinations
    /// (e.g. Ctrl+B for `ToggleTree`) are left alone here and handled by
    /// the normal action-dispatch path instead.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        if ks.modifiers.control || ks.modifiers.platform || ks.modifiers.alt || ks.modifiers.function {
            return;
        }
        if ks.key == "backspace" {
            if self.filter.pop().is_some() {
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if let Some(ch) = &ks.key_char {
            if !ch.is_empty() && ch.chars().all(|c| !c.is_control()) {
                self.filter.push_str(ch);
                cx.notify();
                cx.stop_propagation();
            }
        }
    }
}

impl EventEmitter<TreeEvent> for SchemaTree {}

impl Focusable for SchemaTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

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

impl Render for SchemaTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Sidebar rework: the multi-root flatten, fresh every frame (brief
        // contract #2 unchanged). Pure over entity state — typing in the
        // speed search can never trigger a fetch (design §6, binding).
        let rows = flatten_sidebar(
            &self.grouped,
            &self.conns,
            self.cli_url.as_deref().map(|u| (u, &self.cli_slot)),
            &self.outer_expanded,
            &self.filter,
            self.active_scope.as_ref(),
            &self.favourites,
            self.admin_entry,
            // THE DARK CONTRACT (workspace T3): the „Skripty" section is not
            // emitted yet, so the visible sidebar is byte-identical to
            // pre-T3. Task 7 (the scripts flip) — and ONLY Task 7 — replaces
            // this `None` with `Some((&self.scripts, self.scripts_configured))`.
            None,
        );
        let no_roots = self.grouped.favourites.is_empty()
            && self.grouped.folders.is_empty()
            && self.cli_url.is_none();

        let header_label =
            if self.filter.is_empty() { "Strom schémat".to_string() } else { format!("Strom schémat [{}]", self.filter) };
        // Task 7 contract #3: enabled only when the current selection is a
        // table/view (`selected_table`, not merely `self.selected.is_some()`
        // — e.g. a selected column or routine leaves it disabled).
        let ddl_enabled = self.selected_table().is_some();
        let ddl_color = if ddl_enabled { cx.theme().text_primary } else { cx.theme().border };

        let header = div()
            .h(px(28.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(cx.theme().bg_app)
            .text_color(cx.theme().text_primary)
            .child(div().overflow_hidden().child(header_label))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .id("tree-generate-ddl")
                            .cursor_pointer()
                            .px_1()
                            .text_color(ddl_color)
                            .child("DDL")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.handle_generate_ddl(cx);
                            })),
                    )
                    .child(
                        div()
                            .id("tree-refresh")
                            .cursor_pointer()
                            .px_1()
                            .child("⟳")
                            .on_click(cx.listener(|_this, _: &ClickEvent, _window, cx| {
                                cx.emit(TreeEvent::RefreshRequested);
                            })),
                    ),
            );

        let mut root = div()
            .id("schema-tree")
            .key_context("SchemaTree")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().bg_panel)
            .on_action(cx.listener(Self::on_tree_escape))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .child(header);

        if no_roots {
            // The single-root era's whole-panel loading/error states are
            // GONE — per-row Notice rows carry them now. Only the true
            // empty state remains: no saved connections AND no CLI URL.
            root = root.child(div().px_2().py_1().text_color(cx.theme().text_disabled).child("Bez připojení"));
        } else {
            root = root.child(
                uniform_list(
                    "schema-tree-rows",
                    rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let mut items = Vec::with_capacity(range.len());
                        for ix in range {
                            let (row_id, depth, label, expandable) = rows[ix].clone();
                            let is_expanded = this.row_is_expanded(&row_id);
                            let is_selected = this.selected.as_ref() == Some(&row_id);
                            let chevron = if expandable {
                                if is_expanded { "▾" } else { "▸" }
                            } else {
                                " "
                            };
                            // Design §5 row 1: ambient action icons (★/⊞/⇪)
                            // render ONLY on active-scope rows — cross-
                            // context ambient actions don't exist.
                            let in_scope = row_in_active_scope(&row_id, this.active_scope.as_ref());

                            let click_row = row_id.clone();
                            let chevron_row = row_id.clone();

                            // HONEST INDICATORS (design §1.4): ● = active
                            // context (accent), ○ = inactive Connection row.
                            // There is deliberately NO green/red "connected"
                            // lamp — the runner is per-operation (design
                            // fact 0.1); the two honest indicators are
                            // active context (●) and metadata cached
                            // (children present).
                            let indicator = match &row_id {
                                SidebarRow::Connection { conn_id } => {
                                    let active = match &this.active_scope {
                                        Some(s) => &s.conn_id == conn_id,
                                        None => {
                                            conn_id == crate::CLI_CONN_IDENTITY
                                                && this.cli_url.is_some()
                                        }
                                    };
                                    Some(if active {
                                        ("●", cx.theme().accent)
                                    } else {
                                        ("○", cx.theme().text_disabled)
                                    })
                                }
                                SidebarRow::Database { conn_id, db } => {
                                    let active = this
                                        .active_scope
                                        .as_ref()
                                        .is_some_and(|s| &s.conn_id == conn_id && &s.db == db);
                                    active.then(|| ("●", cx.theme().accent))
                                }
                                _ => None,
                            };
                            let is_active_db = matches!(&row_id, SidebarRow::Database { conn_id, db }
                                if this.active_scope.as_ref().is_some_and(|s| &s.conn_id == conn_id && &s.db == db));

                            // Notice rows: muted informational text, danger
                            // for errors; `retry` rows re-emit their Load
                            // event on click (handle_single_click).
                            let notice_color = match &row_id {
                                SidebarRow::Notice { text, .. } => Some(if text.starts_with("error:") {
                                    cx.theme().danger
                                } else {
                                    cx.theme().text_muted
                                }),
                                _ => None,
                            };

                            // G3 Task 4: ★/☆ toggle — `favourite_object_for`
                            // gates to active-scope Inner rows and pinned
                            // favourite rows itself.
                            let fav_obj = this.favourite_object_for(&row_id);
                            let is_fav = fav_obj.as_ref().is_some_and(|f| this.favourites.contains(f));
                            let star = fav_obj.map(|f| {
                                let (glyph, color) = if is_fav {
                                    ("★", cx.theme().warn)
                                } else {
                                    ("☆", cx.theme().text_disabled)
                                };
                                div()
                                    .id(("tree-star", ix))
                                    .px_1()
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .text_color(color)
                                    .child(glyph)
                                    .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                                        cx.stop_propagation();
                                        cx.emit(TreeEvent::ToggleFavourite(f.clone()));
                                    }))
                            });

                            // G8 T6: ER-diagram entry — active-scope Schema
                            // rows only (design §5 row 1).
                            let diagram_icon = if let (true, SidebarRow::Inner { node: NodeId::Schema(s), .. }) =
                                (in_scope, &row_id)
                            {
                                let schema_for_click =
                                    if s.is_empty() { None } else { Some(s.clone()) };
                                Some(
                                    div()
                                        .id(("tree-erd", ix))
                                        .px_1()
                                        .flex_shrink_0()
                                        .cursor_pointer()
                                        .text_color(cx.theme().accent)
                                        .child("⊞")
                                        .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                                            cx.stop_propagation();
                                            cx.emit(TreeEvent::OpenErDiagram {
                                                schema: schema_for_click.clone(),
                                            });
                                        })),
                                )
                            } else {
                                None
                            };

                            // G12 T4: CSV import — active-scope Table rows,
                            // hidden entirely when read_only (CURATION item
                            // 4(b)'s entry-gate half, unchanged).
                            let csv_icon = if let (true, false, SidebarRow::Inner { node: NodeId::Table(schema, name), .. }) =
                                (in_scope, this.read_only, &row_id)
                            {
                                let schema_for_click =
                                    if schema.is_empty() { None } else { Some(schema.clone()) };
                                let table_for_click = name.clone();
                                Some(
                                    div()
                                        .id(("tree-csv", ix))
                                        .px_1()
                                        .flex_shrink_0()
                                        .cursor_pointer()
                                        .text_color(cx.theme().success)
                                        .child("⇪")
                                        .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                                            cx.stop_propagation();
                                            cx.emit(TreeEvent::ImportCsv {
                                                schema: schema_for_click.clone(),
                                                table: table_for_click.clone(),
                                            });
                                        })),
                                )
                            } else {
                                None
                            };

                            // G10 T4 (design §2): the pinned "Správa serveru"
                            // row — greyed + inline "(pouze pro čtení)" hint
                            // when `Disabled`; click semantics live in
                            // `handle_single_click` (OpenAdmin when Enabled).
                            let is_admin_root = matches!(row_id, SidebarRow::Pinned(NodeId::AdminRoot));
                            let admin_disabled =
                                is_admin_root && this.admin_entry == AdminEntry::Disabled;
                            let label = if admin_disabled {
                                format!("{label} (pouze pro čtení)")
                            } else {
                                label
                            };

                            let mut row = div()
                                .id(("tree-row", ix))
                                .flex()
                                .flex_row()
                                .items_center()
                                .h(px(22.))
                                .pl(px(6. + depth as f32 * 14.))
                                .cursor_pointer()
                                .text_color(if admin_disabled {
                                    cx.theme().text_disabled
                                } else if let Some(c) = notice_color {
                                    c
                                } else {
                                    cx.theme().text_primary
                                })
                                .hover(|s| s.bg(cx.theme().bg_hover));
                            if is_selected || is_active_db {
                                // The active Database row gets the
                                // `bg_selected`-family emphasis alongside
                                // its ● (design §1.4).
                                row = row.bg(cx.theme().bg_selected);
                            }
                            row = row
                                .child(
                                    div()
                                        .id(("tree-chevron", ix))
                                        .w(px(14.))
                                        .flex_shrink_0()
                                        .cursor_pointer()
                                        .child(chevron)
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                            cx.stop_propagation();
                                            this.handle_chevron(&chevron_row, cx);
                                        })),
                                );
                            if let Some((glyph, color)) = indicator {
                                row = row.child(
                                    div()
                                        .w(px(14.))
                                        .flex_shrink_0()
                                        .text_color(color)
                                        .child(glyph),
                                );
                            }
                            row = row
                                .child(div().flex_1().overflow_hidden().child(label))
                                .on_click(cx.listener(move |this, ev: &ClickEvent, _window, cx| {
                                    if ev.click_count() >= 2 {
                                        this.handle_double_click(&click_row, cx);
                                    } else {
                                        this.handle_single_click(&click_row, cx);
                                    }
                                }));
                            if let Some(star) = star {
                                row = row.child(star);
                            }
                            if let Some(icon) = diagram_icon {
                                row = row.child(icon);
                            }
                            if let Some(icon) = csv_icon {
                                row = row.child(icon);
                            }
                            items.push(row);
                        }
                        items
                    }),
                )
                .flex_1(),
            );
        }

        root
    }
}

#[cfg(test)]
mod flatten_tests {
    use super::*;
    use dbc_core::{FkRef, IndexInfo};

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: false, default: None, is_pk: false, fk: None }
    }

    fn table(schema: Option<&str>, name: &str, kind: TableKind, columns: Vec<ColumnInfo>) -> TableInfo {
        TableInfo {
            schema: schema.map(str::to_string),
            name: name.into(),
            kind,
            columns,
            indexes: Vec::new(),
            constraints: Vec::new(),
            ddl: None,
        }
    }

    fn routine(schema: Option<&str>, name: &str, kind: RoutineKind) -> RoutineInfo {
        RoutineInfo { schema: schema.map(str::to_string), name: name.into(), kind, signature: String::new(), ddl: None }
    }

    fn trigger(schema: Option<&str>, name: &str, table: &str) -> TriggerInfo {
        TriggerInfo { schema: schema.map(str::to_string), name: name.into(), table: table.into(), ddl: None }
    }

    fn sequence(schema: Option<&str>, name: &str) -> SequenceInfo {
        SequenceInfo { schema: schema.map(str::to_string), name: name.into() }
    }

    #[test]
    fn sqlite_snapshot_is_a_single_implicit_level_with_no_schema_node() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![col("id", "INTEGER")])],
            ..Default::default()
        };
        let rows = flatten_schema(&snap, &HashSet::new(), "");
        // No `NodeId::Schema` row anywhere, and the section sits at depth 0.
        assert!(!rows.iter().any(|(id, ..)| matches!(id, NodeId::Schema(_))));
        assert_eq!(rows[0].0, NodeId::Section("".to_string(), "Tabulky"));
        assert_eq!(rows[0].1, 0);
    }

    #[test]
    fn multiple_schemas_get_their_own_expandable_schema_node() {
        let snap = SchemaSnapshot {
            tables: vec![
                table(Some("public"), "t1", TableKind::Table, vec![]),
                table(Some("audit"), "t2", TableKind::Table, vec![]),
            ],
            ..Default::default()
        };
        let rows = flatten_schema(&snap, &HashSet::new(), "");
        // Only the two Schema headers show — nothing is expanded yet.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (NodeId::Schema("audit".into()), 0, "audit".into(), true));
        assert_eq!(rows[1], (NodeId::Schema("public".into()), 0, "public".into(), true));

        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Schema("public".into()));
        let rows = flatten_schema(&snap, &expanded, "");
        // "public" expanded reveals its Tabulky section nested one level in;
        // "audit" stays collapsed to just its header.
        assert!(rows.iter().any(|(id, depth, ..)| {
            *id == NodeId::Section("public".into(), "Tabulky") && *depth == 1
        }));
        assert!(!rows.iter().any(|(id, ..)| matches!(id, NodeId::Section(s, _) if s == "audit")));
    }

    #[test]
    fn sections_render_in_fixed_order_with_counts_and_only_when_non_empty() {
        let snap = SchemaSnapshot {
            tables: vec![
                table(None, "a", TableKind::Table, vec![]),
                table(None, "b", TableKind::Table, vec![]),
                table(None, "v1", TableKind::View, vec![]),
            ],
            routines: vec![routine(None, "f1", RoutineKind::Function)],
            triggers: vec![trigger(None, "trg1", "a")],
            sequences: vec![sequence(None, "seq1")],
        };
        let rows = flatten_schema(&snap, &HashSet::new(), "");
        let labels: Vec<&str> = rows.iter().map(|(_, _, label, _)| label.as_str()).collect();
        // Procedury and Indexy are absent (empty); the rest appear in the
        // brief's fixed order, with correct counts.
        assert_eq!(labels, vec!["Tabulky (2)", "Pohledy (1)", "Funkce (1)", "Triggery (1)", "Sekvence (1)"]);
    }

    #[test]
    fn views_and_materialized_views_both_land_in_pohledy() {
        let snap = SchemaSnapshot {
            tables: vec![
                table(None, "v1", TableKind::View, vec![]),
                table(None, "mv1", TableKind::MaterializedView, vec![]),
            ],
            ..Default::default()
        };
        let rows = flatten_schema(&snap, &HashSet::new(), "");
        assert_eq!(rows, vec![(NodeId::Section("".into(), "Pohledy"), 0, "Pohledy (2)".into(), true)]);
    }

    #[test]
    fn expand_collapse_controls_child_visibility() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![col("id", "INTEGER")])],
            ..Default::default()
        };
        // Nothing expanded: only the section header.
        let rows = flatten_schema(&snap, &HashSet::new(), "");
        assert_eq!(rows.len(), 1);

        // Section expanded: table row appears, columns still hidden.
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Section("".into(), "Tabulky"));
        let rows = flatten_schema(&snap, &expanded, "");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].0, NodeId::Table("".into(), "users".into()));

        // Table also expanded: column row appears too.
        expanded.insert(NodeId::Table("".into(), "users".into()));
        let rows = flatten_schema(&snap, &expanded, "");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].0, NodeId::Column("".into(), "users".into(), "id".into()));
        assert_eq!(rows[2].2, "id: INTEGER");
    }

    #[test]
    fn column_label_shows_all_markers_in_order() {
        let c = ColumnInfo {
            name: "user_id".into(),
            data_type: "integer".into(),
            nullable: true,
            default: Some("0".into()),
            is_pk: true,
            fk: Some(FkRef { schema: None, table: "users".into(), column: "id".into() }),
        };
        assert_eq!(column_label(&c), "user_id: integer PK FK ? = 0");
    }

    #[test]
    fn filter_hides_non_matching_and_auto_shows_matching_ancestors() {
        let snap = SchemaSnapshot {
            tables: vec![
                table(None, "users", TableKind::Table, vec![col("id", "INTEGER"), col("email", "TEXT")]),
                table(None, "products", TableKind::Table, vec![col("id", "INTEGER"), col("sku", "TEXT")]),
            ],
            ..Default::default()
        };
        // Filtering by a column name: only "users" (which has a matching
        // column) shows, auto-expanded down to just the matching column —
        // "products" (no match anywhere in it) is hidden entirely, and
        // "id" (present on both tables, doesn't match) doesn't show either
        // since "users" itself didn't match by name.
        let rows = flatten_schema(&snap, &HashSet::new(), "EMAIL");
        assert_eq!(
            rows,
            vec![
                (NodeId::Section("".into(), "Tabulky"), 0, "Tabulky (1)".into(), true),
                (NodeId::Table("".into(), "users".into()), 1, "users".into(), true),
                (NodeId::Column("".into(), "users".into(), "email".into()), 2, "email: TEXT".into(), false),
            ]
        );
    }

    #[test]
    fn filter_matching_table_name_shows_all_of_its_columns() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![col("id", "INTEGER"), col("email", "TEXT")])],
            ..Default::default()
        };
        let rows = flatten_schema(&snap, &HashSet::new(), "users");
        // The table itself matched by name, so both columns show, not just
        // ones whose own name happens to contain "users".
        let col_labels: Vec<&str> = rows
            .iter()
            .filter(|(id, ..)| matches!(id, NodeId::Column(..)))
            .map(|(_, _, l, _)| l.as_str())
            .collect();
        assert_eq!(col_labels, vec!["id: INTEGER", "email: TEXT"]);
    }

    #[test]
    fn indexes_flatten_per_table_as_table_dot_index() {
        let mut t = table(None, "users", TableKind::Table, vec![]);
        t.indexes.push(IndexInfo { name: "users_pkey".into(), columns: vec!["id".into()], unique: true });
        let snap = SchemaSnapshot { tables: vec![t], ..Default::default() };

        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Section("".into(), "Indexy"));
        let rows = flatten_schema(&snap, &expanded, "");
        assert!(rows.iter().any(|(id, depth, label, _)| {
            *id == NodeId::Index("".into(), "users".into(), "users_pkey".into())
                && *depth == 1
                && label == "users.users_pkey"
        }));
    }

    #[test]
    fn empty_snapshot_flattens_to_no_rows() {
        let snap = SchemaSnapshot::default();
        assert!(flatten_schema(&snap, &HashSet::new(), "").is_empty());
    }

    // --- review Issue 3: same-connection refresh state preservation ---

    #[test]
    fn all_node_ids_covers_every_kind_including_indexes_and_multi_schema() {
        let mut t = table(Some("public"), "users", TableKind::Table, vec![col("id", "INTEGER")]);
        t.indexes.push(IndexInfo { name: "users_pkey".into(), columns: vec!["id".into()], unique: true });
        let snap = SchemaSnapshot {
            tables: vec![t],
            routines: vec![routine(Some("public"), "f1", RoutineKind::Function)],
            triggers: vec![trigger(Some("public"), "trg1", "users")],
            sequences: vec![sequence(Some("public"), "seq1")],
        };
        let ids = all_node_ids(&snap);
        assert!(ids.contains(&NodeId::Schema("public".into())));
        assert!(ids.contains(&NodeId::Section("public".into(), "Tabulky")));
        assert!(ids.contains(&NodeId::Table("public".into(), "users".into())));
        assert!(ids.contains(&NodeId::Column("public".into(), "users".into(), "id".into())));
        assert!(ids.contains(&NodeId::Section("public".into(), "Indexy")));
        assert!(ids.contains(&NodeId::Index("public".into(), "users".into(), "users_pkey".into())));
        assert!(ids.contains(&NodeId::Section("public".into(), "Funkce")));
        assert!(ids.contains(&NodeId::Routine("public".into(), "f1".into())));
        assert!(ids.contains(&NodeId::Section("public".into(), "Triggery")));
        assert!(ids.contains(&NodeId::Trigger("public".into(), "trg1".into())));
        assert!(ids.contains(&NodeId::Section("public".into(), "Sekvence")));
        assert!(ids.contains(&NodeId::Sequence("public".into(), "seq1".into())));
    }

    #[test]
    fn all_node_ids_omits_schema_node_for_single_implicit_level() {
        let snap = SchemaSnapshot { tables: vec![table(None, "users", TableKind::Table, vec![])], ..Default::default() };
        let ids = all_node_ids(&snap);
        assert!(!ids.iter().any(|id| matches!(id, NodeId::Schema(_))));
    }

    #[test]
    fn prune_stale_ids_keeps_valid_and_drops_missing_expanded_and_selected() {
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Section("".into(), "Tabulky"));
        expanded.insert(NodeId::Table("".into(), "users".into()));
        expanded.insert(NodeId::Table("".into(), "products".into()));
        let selected = Some(NodeId::Table("".into(), "products".into()));

        // Refreshed snapshot: "products" table is gone (dropped since the
        // last fetch), "users" survives (with an extra column, which
        // doesn't matter for pruning purposes).
        let new_snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![col("id", "INTEGER"), col("email", "TEXT")])],
            ..Default::default()
        };

        let (pruned_expanded, pruned_selected) = prune_stale_ids(&expanded, &selected, &new_snap);
        assert!(pruned_expanded.contains(&NodeId::Section("".into(), "Tabulky")));
        assert!(pruned_expanded.contains(&NodeId::Table("".into(), "users".into())));
        assert!(!pruned_expanded.contains(&NodeId::Table("".into(), "products".into())));
        assert_eq!(pruned_expanded.len(), 2);
        // The selected node ("products") no longer exists in the new
        // snapshot, so it's cleared rather than pointing at a dead id.
        assert_eq!(pruned_selected, None);
    }

    #[test]
    fn prune_stale_ids_keeps_selected_and_expanded_when_still_present() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![col("id", "INTEGER")])],
            ..Default::default()
        };
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Section("".into(), "Tabulky"));
        expanded.insert(NodeId::Table("".into(), "users".into()));
        let selected = Some(NodeId::Table("".into(), "users".into()));

        let (pruned_expanded, pruned_selected) = prune_stale_ids(&expanded, &selected, &snap);
        assert_eq!(pruned_expanded, expanded);
        assert_eq!(pruned_selected, selected);
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;
    use dbc_state::ConnectionConfig;
    use dbc_state::Engine;
    use std::collections::HashMap;

    fn conn_cfg(id: &str, name: &str, folder: &[&str], engine: Engine, db: &str) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(), name: name.into(),
            folder: folder.iter().map(|s| s.to_string()).collect(),
            engine, host: "h".into(), port: None, database: db.into(),
            user: "u".into(), read_only: false, timeout_secs: None,
            auto_limit: None, ssh: None, favourite: false, mssql: None,
        }
    }

    fn grouped(conns: &[ConnectionConfig]) -> crate::connections_ui::GroupedConnections {
        crate::connections_ui::group_connections(conns)
    }

    /// A tiny one-schema/one-table snapshot, same shape flatten_tests uses.
    fn snap() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![TableInfo {
                schema: Some("public".into()), name: "orders".into(),
                kind: TableKind::Table,
                columns: vec![ColumnInfo {
                    name: "id".into(), data_type: "integer".into(),
                    nullable: false, is_pk: true, fk: None, default: None,
                }],
                indexes: vec![], constraints: vec![], ddl: None,
            }],
            routines: vec![], triggers: vec![], sequences: vec![],
        }
    }

    fn loaded_states(conn_id: &str, db: &str) -> HashMap<String, ConnNode> {
        let mut m = HashMap::new();
        m.insert(conn_id.to_string(), ConnNode {
            dbs: DbListState::Loaded {
                dbs: vec![DbNode {
                    name: db.to_string(), is_default: true,
                    schema: DbSchemaState::Loaded { snapshot: snap(), expanded: HashSet::new() },
                }],
                truncated: false,
            },
        });
        m
    }

    #[test]
    fn folder_connection_database_depths_compose() {
        let conns = vec![conn_cfg("c1", "prod-pg", &["work"], Engine::Postgres, "sales")];
        let mut outer = HashSet::new();
        // Folders default OPEN (OuterId::Folder in the set means COLLAPSED
        // — see the polarity note on `OuterId`), so "work" is NOT inserted.
        outer.insert(OuterId::Connection("c1".into()));
        outer.insert(OuterId::Database("c1".into(), "sales".into()));
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", "sales"), None,
            &outer, "", None, &[], AdminEntry::Hidden, None);
        // folder(0) -> connection(1) -> database(2) -> spliced schema rows (3+)
        assert!(matches!(&rows[0], (SidebarRow::Folder { path }, 0, _, true) if path == &vec!["work".to_string()]));
        assert!(matches!(&rows[1], (SidebarRow::Connection { conn_id }, 1, _, true) if conn_id == "c1"));
        assert!(matches!(&rows[2], (SidebarRow::Database { conn_id, db }, 2, label, true)
            if conn_id == "c1" && db == "sales" && label.contains("(výchozí)")));
        assert!(matches!(&rows[3], (SidebarRow::Inner { conn_id, db, node: NodeId::Schema(_) }, 3, _, _)
            if conn_id == "c1" && db == "sales"));
    }

    /// Pins the polarity asymmetry doc'd on `OuterId`: presence of
    /// `OuterId::Folder` in the set means COLLAPSED (folders default open;
    /// connections/databases default closed — presence means expanded).
    #[test]
    fn folder_in_outer_set_is_collapsed() {
        let conns = vec![conn_cfg("c1", "prod-pg", &["work"], Engine::Postgres, "sales")];
        let mut outer = HashSet::new();
        outer.insert(OuterId::Folder(vec!["work".into()]));
        outer.insert(OuterId::Connection("c1".into()));
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", "sales"), None,
            &outer, "", None, &[], AdminEntry::Hidden, None);
        assert_eq!(rows.len(), 1, "collapsed folder shows only its own row");
        assert!(matches!(&rows[0], (SidebarRow::Folder { .. }, 0, _, true)));
    }

    #[test]
    fn loose_connections_sit_at_depth_zero() {
        let conns = vec![conn_cfg("c1", "loose", &[], Engine::Postgres, "db")];
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden, None);
        assert!(matches!(&rows[0], (SidebarRow::Connection { .. }, 0, _, true)));
    }

    #[test]
    fn collapsed_connection_hides_children_but_keeps_cache() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let states = loaded_states("c1", "sales");
        // NOT in outer_expanded -> only the connection row renders; the
        // Loaded cache is untouched (re-expand is instant by construction).
        let rows = flatten_sidebar(&grouped(&conns), &states, None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden, None);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn lazy_states_render_their_notice_rows() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let mut outer = HashSet::new();
        outer.insert(OuterId::Connection("c1".into()));
        for (state, expect_text, expect_retry) in [
            (DbListState::Loading { generation: 1 }, "Načítám databáze…", false),
            (DbListState::Error("kaput".into()), "error: kaput", true),
        ] {
            let mut states = HashMap::new();
            states.insert("c1".into(), ConnNode { dbs: state });
            let rows = flatten_sidebar(&grouped(&conns), &states, None,
                &outer, "", None, &[], AdminEntry::Hidden, None);
            assert!(matches!(&rows[1], (SidebarRow::Notice { conn_id, db: None, text, retry }, 1, _, false)
                if conn_id == "c1" && text == expect_text && *retry == expect_retry),
                "state expecting {expect_text}: got {:?}", rows.get(1));
        }
    }

    #[test]
    fn truncation_notice_renders_after_the_db_rows() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let mut states = loaded_states("c1", "sales");
        if let DbListState::Loaded { truncated, .. } = &mut states.get_mut("c1").unwrap().dbs {
            *truncated = true;
        }
        let mut outer = HashSet::new();
        outer.insert(OuterId::Connection("c1".into()));
        let rows = flatten_sidebar(&grouped(&conns), &states, None,
            &outer, "", None, &[], AdminEntry::Hidden, None);
        let last = rows.last().unwrap();
        assert!(matches!(&last.0, SidebarRow::Notice { retry: false, text, .. }
            if text == "… zobrazeno prvních 2000 databází — použijte výchozí databázi nebo filtr"));
    }

    #[test]
    fn file_engine_db_row_keeps_spec_path_but_displays_stem() {
        // Resolved deviation 5: the row's identity is the SPEC string; the
        // label is the stem. One uniform hierarchy — the Database row stays
        // (it is the double-click switch target).
        assert_eq!(display_db_name(Engine::Duckdb, r"D:\data\analytics.duckdb"), "analytics");
        assert_eq!(display_db_name(Engine::Sqlite, r"D:\x\app.db"), "app");
        assert_eq!(display_db_name(Engine::Postgres, "sales"), "sales");
        let conns = vec![conn_cfg("c1", "duck", &[], Engine::Duckdb, r"D:\data\analytics.duckdb")];
        let mut outer = HashSet::new();
        outer.insert(OuterId::Connection("c1".into()));
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", r"D:\data\analytics.duckdb"),
            None, &outer, "", None, &[], AdminEntry::Hidden, None);
        assert!(matches!(&rows[1], (SidebarRow::Database { db, .. }, 1, label, true)
            if db == r"D:\data\analytics.duckdb" && label.starts_with("analytics")));
    }

    #[test]
    fn cli_root_splices_schema_directly_without_a_database_level() {
        // Resolved deviation 12: no db switching on the CLI path => no dead
        // switch-target row.
        let slot = DbSchemaState::Loaded { snapshot: snap(), expanded: HashSet::new() };
        let mut outer = HashSet::new();
        outer.insert(OuterId::Connection(crate::CLI_CONN_IDENTITY.to_string()));
        let rows = flatten_sidebar(&grouped(&[]), &HashMap::new(),
            Some(("postgres://localhost/x", &slot)), &outer, "", None, &[], AdminEntry::Hidden, None);
        assert!(matches!(&rows[0], (SidebarRow::Connection { conn_id }, 0, label, true)
            if conn_id == crate::CLI_CONN_IDENTITY && label == "postgres://localhost/x"));
        assert!(matches!(&rows[1], (SidebarRow::Inner { db, .. }, 1, _, _) if db.is_empty()));
    }

    #[test]
    fn pinned_admin_and_favourites_render_once_scoped_to_active_context() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let favs = vec![
            FavouriteObject { connection_id: "c1".into(), schema: Some("public".into()),
                name: "orders".into(), kind: "table".into(), database: None },
            FavouriteObject { connection_id: "c1".into(), schema: Some("public".into()),
                name: "other".into(), kind: "table".into(), database: Some("inventory".into()) },
        ];
        let scope = ActiveScope { conn_id: "c1".into(), db: "sales".into(), default_db: "sales".into() };
        let mut outer = HashSet::new();
        outer.insert(OuterId::Favourites);
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &outer, "", Some(&scope), &favs, AdminEntry::Enabled, None);
        assert!(matches!(&rows[0], (SidebarRow::Pinned(NodeId::AdminRoot), 0, _, false)));
        // Only the default-db favourite is in scope (database: None == default):
        assert!(matches!(&rows[1], (SidebarRow::Pinned(NodeId::FavouriteSection), 0, label, true)
            if label == "Oblíbené (1)"));
        assert!(matches!(&rows[2], (SidebarRow::Pinned(NodeId::Favourite(..)), 1, label, false)
            if label == "public.orders"));
    }

    /// Moved from the deleted single-root `flatten` tests (T5): the pinned
    /// favourite rows' `NodeId::Favourite(kind, schema, name)` carries the
    /// stored `kind` unchanged — exactly the data `handle_double_click`'s
    /// `Pinned(Favourite)` arm switches on (OpenPreview for table/view,
    /// OpenDdl for routine/trigger, no-op for sequence).
    #[test]
    fn pinned_favourite_node_ids_carry_kind_for_double_click_dispatch() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let favs: Vec<FavouriteObject> = ["table", "view", "routine", "trigger", "sequence"]
            .iter()
            .map(|kind| FavouriteObject {
                connection_id: "c1".into(),
                schema: Some("s".into()),
                name: format!("a_{kind}"),
                kind: kind.to_string(),
                database: None,
            })
            .collect();
        let scope = ActiveScope { conn_id: "c1".into(), db: "sales".into(), default_db: "sales".into() };
        let mut outer = HashSet::new();
        outer.insert(OuterId::Favourites);
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &outer, "", Some(&scope), &favs, AdminEntry::Hidden, None);
        // rows[0] = the section header; the connection root follows the
        // favourite items — take exactly the five item rows.
        let ids: Vec<&SidebarRow> = rows.iter().skip(1).take(5).map(|(id, ..)| id).collect();
        let want: Vec<SidebarRow> = ["table", "view", "routine", "trigger", "sequence"]
            .iter()
            .map(|kind| {
                SidebarRow::Pinned(NodeId::Favourite(kind.to_string(), "s".into(), format!("a_{kind}")))
            })
            .collect();
        assert_eq!(ids, want.iter().collect::<Vec<_>>());
    }

    // T7 re-pins: three favourites-section behaviors whose single-root
    // pins were thinned when the old `flatten` tests were deleted in T5.

    /// The "Oblíbené" section is ABSENT without an active scope (CLI /
    /// no-connection: nothing to stamp a new toggle with, nothing to
    /// filter against) — even with favourites configured.
    #[test]
    fn favourites_section_absent_without_active_scope() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let favs = vec![FavouriteObject {
            connection_id: "c1".into(),
            schema: Some("public".into()),
            name: "orders".into(),
            kind: "table".into(),
            database: None,
        }];
        let mut outer = HashSet::new();
        outer.insert(OuterId::Favourites);
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &outer, "", None, &favs, AdminEntry::Hidden, None);
        assert!(!rows
            .iter()
            .any(|(id, ..)| matches!(id, SidebarRow::Pinned(NodeId::FavouriteSection))));
    }

    /// The section renders collapsed by default — header only; item rows
    /// appear only once `OuterId::Favourites` is in the expand set.
    #[test]
    fn favourites_section_stays_collapsed_until_expanded() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let favs = vec![FavouriteObject {
            connection_id: "c1".into(),
            schema: Some("public".into()),
            name: "orders".into(),
            kind: "table".into(),
            database: None,
        }];
        let scope = ActiveScope { conn_id: "c1".into(), db: "sales".into(), default_db: "sales".into() };
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &HashSet::new(), "", Some(&scope), &favs, AdminEntry::Hidden, None);
        assert!(matches!(&rows[0], (SidebarRow::Pinned(NodeId::FavouriteSection), 0, label, true)
            if label == "Oblíbené (1)"));
        assert!(
            !rows.iter().any(|(id, ..)| matches!(id, SidebarRow::Pinned(NodeId::Favourite(..)))),
            "item rows hidden until the section is expanded"
        );
    }

    /// Item labels: „{schema}.{name}" with a schema, bare „{name}" without.
    #[test]
    fn favourites_section_labels_schema_dot_name_or_bare_name() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let favs = vec![
            FavouriteObject {
                connection_id: "c1".into(),
                schema: Some("public".into()),
                name: "t1".into(),
                kind: "table".into(),
                database: None,
            },
            FavouriteObject {
                connection_id: "c1".into(),
                schema: None,
                name: "seq1".into(),
                kind: "sequence".into(),
                database: None,
            },
        ];
        let scope = ActiveScope { conn_id: "c1".into(), db: "sales".into(), default_db: "sales".into() };
        let mut outer = HashSet::new();
        outer.insert(OuterId::Favourites);
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &outer, "", Some(&scope), &favs, AdminEntry::Hidden, None);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|(id, ..)| matches!(id, SidebarRow::Pinned(NodeId::Favourite(..))))
            .map(|(_, _, l, _)| l.as_str())
            .collect();
        assert_eq!(labels, vec!["public.t1", "seq1"]);
    }

    /// Design §5 row 1's REQUIRED "active-scope gating of icon
    /// affordances" test — the pure predicate T5's render uses to decide
    /// whether a row gets the star/ER/import icons and DDL-header
    /// enablement. Cross-context ambient actions must simply not exist.
    #[test]
    fn icon_affordances_gate_on_active_scope() {
        let scope = ActiveScope { conn_id: "c1".into(), db: "sales".into(), default_db: "sales".into() };
        let node = NodeId::Table("public".into(), "orders".into());
        let in_scope = SidebarRow::Inner { conn_id: "c1".into(), db: "sales".into(), node: node.clone() };
        let other_db = SidebarRow::Inner { conn_id: "c1".into(), db: "inventory".into(), node: node.clone() };
        let other_conn = SidebarRow::Inner { conn_id: "c2".into(), db: "sales".into(), node };
        assert!(row_in_active_scope(&in_scope, Some(&scope)));
        assert!(!row_in_active_scope(&other_db, Some(&scope)));
        assert!(!row_in_active_scope(&other_conn, Some(&scope)));
        assert!(!row_in_active_scope(&in_scope, None));
        // CLI rows stay active-context when no saved connection is:
        let cli_row = SidebarRow::Inner {
            conn_id: crate::CLI_CONN_IDENTITY.into(), db: String::new(),
            node: NodeId::Table("".into(), "t".into()),
        };
        assert!(row_in_active_scope(&cli_row, None));
        assert!(!row_in_active_scope(&cli_row, Some(&scope)));
        // Pinned rows are active-context by definition:
        assert!(row_in_active_scope(&SidebarRow::Pinned(NodeId::AdminRoot), Some(&scope)));
        // Structural rows never carry ambient icons:
        assert!(!row_in_active_scope(&SidebarRow::Connection { conn_id: "c1".into() }, Some(&scope)));
    }

    #[test]
    fn favourite_in_scope_resolves_none_as_default_db() {
        let scope = ActiveScope { conn_id: "c1".into(), db: "inventory".into(), default_db: "sales".into() };
        let f = |database: Option<&str>| FavouriteObject {
            connection_id: "c1".into(), schema: None, name: "t".into(),
            kind: "table".into(), database: database.map(String::from),
        };
        assert!(!favourite_in_scope(&f(None), &scope));            // default (= sales) != inventory
        assert!(favourite_in_scope(&f(Some("inventory")), &scope));
        let default_scope = ActiveScope { conn_id: "c1".into(), db: "sales".into(), default_db: "sales".into() };
        assert!(favourite_in_scope(&f(None), &default_scope));
    }

    #[test]
    fn filter_narrows_loaded_content_and_matches_row_labels() {
        let conns = vec![
            conn_cfg("c1", "prod-pg", &[], Engine::Postgres, "sales"),
            conn_cfg("c2", "staging", &[], Engine::Postgres, "sales"),
        ];
        let mut outer = HashSet::new();
        outer.insert(OuterId::Connection("c1".into()));
        outer.insert(OuterId::Database("c1".into(), "sales".into()));
        // "orders" matches only inside c1's loaded snapshot: c1's chain
        // stays visible (ancestors auto-show), c2's row (label-only miss,
        // nothing loaded) is hidden. Filtering NEVER fetches — pure fn,
        // holds by construction.
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", "sales"), None,
            &outer, "orders", None, &[], AdminEntry::Hidden, None);
        assert!(rows.iter().any(|r| matches!(&r.0, SidebarRow::Connection { conn_id } if conn_id == "c1")));
        assert!(!rows.iter().any(|r| matches!(&r.0, SidebarRow::Connection { conn_id } if conn_id == "c2")));
        assert!(rows.iter().any(|r| matches!(&r.0, SidebarRow::Inner { node: NodeId::Table(_, t), .. } if t == "orders")));
        // A filter matching a connection's own NAME keeps its row visible:
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", "sales"), None,
            &outer, "staging", None, &[], AdminEntry::Hidden, None);
        assert!(rows.iter().any(|r| matches!(&r.0, SidebarRow::Connection { conn_id } if conn_id == "c2")));
    }

    // ---------- state-machine transitions ----------

    #[test]
    fn schema_load_carries_expansion_through_refresh_and_prunes_stale() {
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Schema("public".into()));
        expanded.insert(NodeId::Table("public".into(), "vanished".into()));
        let mut slot = DbSchemaState::Loaded { snapshot: snap(), expanded };
        begin_schema_load(&mut slot, 7);
        assert!(matches!(&slot, DbSchemaState::Loading { generation: 7, prev_expanded } if prev_expanded.len() == 2));
        apply_schema_result(&mut slot, 7, Ok(snap()));
        let DbSchemaState::Loaded { expanded, .. } = &slot else { panic!("Loaded expected") };
        assert!(expanded.contains(&NodeId::Schema("public".into())));
        assert!(!expanded.contains(&NodeId::Table("public".into(), "vanished".into())),
            "stale ids must be pruned (design §1.2 carry-forward)");
    }

    #[test]
    fn stale_generation_results_are_dropped() {
        let mut slot = DbSchemaState::NotLoaded;
        begin_schema_load(&mut slot, 1);
        begin_schema_load(&mut slot, 2); // superseding dispatch
        apply_schema_result(&mut slot, 1, Ok(snap()));
        assert!(matches!(&slot, DbSchemaState::Loading { generation: 2, .. }),
            "gen-1 result must not clobber the gen-2 in-flight state");
        apply_schema_result(&mut slot, 2, Err("boom".into()));
        assert!(matches!(&slot, DbSchemaState::Error(e) if e == "boom"));
    }

    #[test]
    fn db_list_result_marks_default_and_truncation() {
        let mut node = ConnNode { dbs: DbListState::NotLoaded };
        begin_db_list_load(&mut node, 3);
        apply_db_list_result(&mut node, 3,
            Ok((vec!["inventory".into(), "sales".into()], true)), "sales");
        let DbListState::Loaded { dbs, truncated } = &node.dbs else { panic!() };
        assert!(truncated);
        assert_eq!(dbs.iter().map(|d| (d.name.as_str(), d.is_default)).collect::<Vec<_>>(),
            vec![("inventory", false), ("sales", true)]);
        assert!(dbs.iter().all(|d| matches!(d.schema, DbSchemaState::NotLoaded)));
    }

    /// T5 review MAJOR 1: a switch (dropdown/palette/double-click) to a
    /// connection whose db list was never expanded must land its schema
    /// fetch in the active-context fallback slot — pre-fix, `slot_mut`
    /// found nothing and the snapshot was silently DROPPED (autocomplete,
    /// fk joins, `detect_editable_pk`, palette and the admin seed all
    /// degraded until a manual double expand).
    #[test]
    fn switch_to_unexpanded_connection_lands_schema_in_the_fallback_slot() {
        let mut slot: ActiveSlot = None;
        begin_fallback_schema_load(&mut slot, "c1", "sales", 7);
        assert!(matches!(fallback_slot(&slot, "c1", "sales"), Some(DbSchemaState::Loading { .. })));
        apply_fallback_schema_result(&mut slot, "c1", "sales", 7, Ok(snap()));
        let Some(DbSchemaState::Loaded { snapshot, .. }) = fallback_slot(&slot, "c1", "sales")
        else {
            panic!("active-scope switch fetch must populate the fallback slot");
        };
        assert_eq!(snapshot.tables[0].name, "orders");
        // Stale generation still drops (same contract as the map slots):
        apply_fallback_schema_result(&mut slot, "c1", "sales", 6, Err("stale".into()));
        assert!(matches!(fallback_slot(&slot, "c1", "sales"), Some(DbSchemaState::Loaded { .. })));
    }

    /// T5 review MAJOR 1 (+ the T7 census pre-pin): the fallback is
    /// key-gated — a result for another `(conn, db)` never lands in it,
    /// and lookups for another scope never read from it.
    #[test]
    fn fallback_slot_is_key_gated_no_cross_context_leak() {
        let mut slot: ActiveSlot = None;
        begin_fallback_schema_load(&mut slot, "c1", "sales", 1);
        // Results for a DIFFERENT scope must not land:
        apply_fallback_schema_result(&mut slot, "c2", "sales", 1, Ok(snap()));
        apply_fallback_schema_result(&mut slot, "c1", "inventory", 1, Ok(snap()));
        assert!(matches!(fallback_slot(&slot, "c1", "sales"), Some(DbSchemaState::Loading { .. })));
        // Loaded for (c1, sales) answers ONLY (c1, sales):
        apply_fallback_schema_result(&mut slot, "c1", "sales", 1, Ok(snap()));
        assert!(fallback_slot(&slot, "c1", "sales").is_some());
        assert!(fallback_slot(&slot, "c1", "inventory").is_none());
        assert!(fallback_slot(&slot, "c2", "sales").is_none());
    }

    /// T5 review MAJOR 1: once the db list loads, the fallback schema
    /// migrates into its real `DbNode` (the fallback empties; a `Loaded`
    /// migration returns its key for LRU accounting). A list that does not
    /// contain the fallback's db — or a different connection's list —
    /// leaves the fallback in place.
    #[test]
    fn db_list_load_migrates_the_fallback_schema_into_its_db_node() {
        let mut slot: ActiveSlot = None;
        begin_fallback_schema_load(&mut slot, "c1", "sales", 1);
        apply_fallback_schema_result(&mut slot, "c1", "sales", 1, Ok(snap()));
        let mut node = ConnNode { dbs: DbListState::NotLoaded };
        begin_db_list_load(&mut node, 2);
        apply_db_list_result(
            &mut node,
            2,
            Ok((vec!["postgres".into(), "sales".into()], false)),
            "postgres",
        );
        assert_eq!(
            migrate_fallback_into_list(&mut node, &mut slot, "c1"),
            Some(("c1".into(), "sales".into()))
        );
        assert!(slot.is_none(), "fallback empties after migration");
        let DbListState::Loaded { dbs, .. } = &node.dbs else { panic!("list loaded above") };
        let d = dbs.iter().find(|d| d.name == "sales").unwrap();
        assert!(
            matches!(&d.schema, DbSchemaState::Loaded { snapshot, .. } if snapshot.tables[0].name == "orders"),
            "the loaded schema must survive the migration"
        );
        // A list without the fallback's db (e.g. truncated) keeps it:
        let mut slot2: ActiveSlot = None;
        begin_fallback_schema_load(&mut slot2, "c1", "gone", 3);
        apply_fallback_schema_result(&mut slot2, "c1", "gone", 3, Ok(snap()));
        assert!(migrate_fallback_into_list(&mut node, &mut slot2, "c1").is_none());
        assert!(slot2.is_some(), "active context must not lose its schema to a listing artifact");
        // A different connection's list load leaves it untouched too:
        assert!(migrate_fallback_into_list(&mut node, &mut slot2, "c2").is_none());
        assert!(slot2.is_some());
    }

    #[test]
    fn lru_evicts_oldest_loaded_but_never_the_active_slot() {
        let mut states: HashMap<String, ConnNode> = HashMap::new();
        let mut lru: Vec<(String, String)> = Vec::new();
        // Load CAP + 2 slots on one connection.
        let db_names: Vec<String> = (0..LOADED_SNAPSHOT_CAP + 2).map(|i| format!("db{i}")).collect();
        states.insert("c1".into(), ConnNode {
            dbs: DbListState::Loaded {
                dbs: db_names.iter().map(|n| DbNode {
                    name: n.clone(), is_default: n == "db0",
                    schema: DbSchemaState::Loaded { snapshot: snap(), expanded: HashSet::new() },
                }).collect(),
                truncated: false,
            },
        });
        let active = ("c1".to_string(), "db0".to_string());
        for n in &db_names {
            touch_and_evict(&mut states, &mut lru, ("c1".into(), n.clone()), Some(&active));
        }
        let DbListState::Loaded { dbs, .. } = &states["c1"].dbs else { panic!() };
        let loaded: Vec<&str> = dbs.iter()
            .filter(|d| matches!(d.schema, DbSchemaState::Loaded { .. }))
            .map(|d| d.name.as_str()).collect();
        assert!(loaded.len() <= LOADED_SNAPSHOT_CAP);
        assert!(loaded.contains(&"db0"), "the ACTIVE slot is never evicted");
        // The oldest non-active touches (db1, db2) were evicted back to NotLoaded:
        assert!(!loaded.contains(&"db1"));
        // Re-touching moves to the back — touch db3 again, then overflow once more:
        touch_and_evict(&mut states, &mut lru, ("c1".into(), "db3".into()), Some(&active));
    }

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
        let conns = vec![conn_cfg("c1", "Prod", &[], Engine::Postgres, "sales")];
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
        let conns = vec![conn_cfg("c1", "Prod", &[], Engine::Postgres, "sales")];
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
}
