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
//   3. `SchemaTree` — the GPUI entity: owns the snapshot/expanded/filter/
//      selection/loading/error state, renders via `uniform_list` (calling
//      `flatten` fresh every frame — brief contract #2), and emits
//      `TreeEvent`s for the things it can't handle itself (opening a
//      preview/DDL tab, asking to be refreshed).
//
// Fetch-lifecycle state (`loading`/`error`/`snapshot`) is driven by direct
// entity mutation from `main.rs` (`set_loading`/`set_snapshot`/`set_error`/
// `clear`), NOT by `TreeEvent` — the brief's `TreeEvent` enum only has
// `OpenPreview`/`OpenDdl`/`RefreshRequested`, so lifecycle transitions have
// no event variant to ride; `main.rs` already owns the `QueryRunner` and the
// active connection spec, so it's the natural owner of "start a fetch,
// update the tree entity when it resolves" too (see
// `AppView::trigger_schema_fetch`).

use std::collections::{BTreeSet, HashSet};

use dbc_core::{
    synthesize_create_table, ColumnInfo, RoutineInfo, RoutineKind, SchemaSnapshot, SequenceInfo,
    TableInfo, TableKind, TriggerInfo,
};
use dbc_state::FavouriteObject;

use crate::admin_panel::AdminEntry;
use gpui::{
    actions, div, prelude::*, px, rgb, uniform_list, App, ClickEvent, Context, EventEmitter,
    FocusHandle, Focusable, KeyBinding, KeyDownEvent, MouseButton, Window,
};

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
    OpenPreview { schema: Option<String>, table: String },
    OpenDdl { title: String, ddl: String },
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
fn emit_favourites_section(
    out: &mut Vec<FlatNode>,
    favourites: &[FavouriteObject],
    active_connection_id: Option<&str>,
    expanded: &HashSet<NodeId>,
) {
    let Some(conn_id) = active_connection_id else { return };
    let items: Vec<&FavouriteObject> = favourites.iter().filter(|f| f.connection_id == conn_id).collect();
    if items.is_empty() {
        return;
    }
    let section_id = NodeId::FavouriteSection;
    out.push((section_id.clone(), 0, format!("Oblíbené ({})", items.len()), true));
    if !expanded.contains(&section_id) {
        return;
    }
    for f in items {
        let schema_key = f.schema.clone().unwrap_or_default();
        let label = if schema_key.is_empty() { f.name.clone() } else { format!("{}.{}", schema_key, f.name) };
        let id = NodeId::Favourite(f.kind.clone(), schema_key, f.name.clone());
        out.push((id, 1, label, false));
    }
}

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

/// Pure, GPUI-free: computes exactly the visible rows for the current
/// `snapshot`/`expanded`/`filter`. Called fresh every render (brief
/// contract #2 — rows are cheap, snapshots can be thousands of objects).
///
/// Schema grouping (contract #3): when every table/routine/trigger/sequence
/// in `snapshot` has `schema: None` (SQLite has no schema concept), the
/// schema level is a single implicit level and is omitted entirely —
/// sections render straight at depth 0. Otherwise each distinct schema gets
/// its own expandable `Schema` node at depth 0, with sections nested one
/// level deeper once that schema is expanded.
pub fn flatten(
    snapshot: &SchemaSnapshot,
    expanded: &HashSet<NodeId>,
    filter: &str,
    favourites: &[FavouriteObject],
    active_connection_id: Option<&str>,
    admin: AdminEntry,
) -> Vec<FlatNode> {
    let mut out = Vec::new();
    let filter_lc = filter.to_lowercase();
    let filter_active = !filter_lc.is_empty();

    // G10 T4 (design §2): the pinned "Správa serveru" row, when not
    // Hidden, renders FIRST — above even "Oblíbené" below. Never
    // expandable (`false`). Its greyed/disabled `Disabled` rendering lives
    // in `SchemaTree::render` (this function only decides visibility, not
    // styling).
    if admin != AdminEntry::Hidden {
        out.push((NodeId::AdminRoot, 0, "Správa serveru".to_string(), false));
    }

    // G3 Task 4: the "Oblíbené" section always comes first, ahead of any
    // schema/section node below.
    emit_favourites_section(&mut out, favourites, active_connection_id, expanded);

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
    snapshot: Option<SchemaSnapshot>,
    expanded: HashSet<NodeId>,
    filter: String,
    selected: Option<NodeId>,
    loading: bool,
    error: Option<String>,
    focus_handle: FocusHandle,
    /// The SQL editor's focus handle, handed in by `main.rs` at construction
    /// (it owns both entities) so `on_tree_escape` can blur the tree back to
    /// the editor directly, without needing a `TreeEvent` round-trip through
    /// `AppView` (which doesn't have `Window` access in its `cx.subscribe`
    /// callback) — see `on_tree_escape`.
    editor_focus: FocusHandle,
    /// G3 Task 4: `AppConfig::favourite_objects`, pushed in by `main.rs`
    /// (`set_favourites`) on every schema-fetch apply and after every ★
    /// toggle — NOT filtered by connection here; `flatten`/`favourite_object_for`
    /// do that filtering against `active_connection_id` below.
    favourites: Vec<FavouriteObject>,
    /// The active connection's id (`AppView::active_connection_id`), handed
    /// in alongside `favourites` by `set_favourites` — `None` for the
    /// CLI-arg URL path (no `ConnectionConfig`/id to match favourites
    /// against, so the "Oblíbené" section stays hidden) or before any
    /// connection has been chosen.
    active_connection_id: Option<String>,
    /// G12 T4: `AppView::active_read_only()`, pushed in alongside every
    /// snapshot/favourites update (`main.rs::trigger_schema_fetch`) — gates
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
}

impl SchemaTree {
    pub fn new(cx: &mut Context<Self>, editor_focus: FocusHandle) -> Self {
        Self {
            snapshot: None,
            expanded: HashSet::new(),
            filter: String::new(),
            selected: None,
            loading: false,
            error: None,
            focus_handle: cx.focus_handle(),
            editor_focus,
            favourites: Vec::new(),
            active_connection_id: None,
            read_only: false,
            admin_entry: AdminEntry::Hidden,
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

    /// Called by `main.rs` on every schema-fetch apply (`trigger_schema_fetch`)
    /// and again right after a ★ toggle resolves (`config.toggle_favourite` +
    /// guarded save) — `flatten`'s "Oblíbené" section and every row's star
    /// state are recomputed fresh from these two fields on the very next
    /// render, same as `snapshot`/`expanded`/`filter`.
    pub fn set_favourites(
        &mut self,
        favourites: Vec<FavouriteObject>,
        active_connection_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.favourites = favourites;
        self.active_connection_id = active_connection_id;
        cx.notify();
    }

    /// The `FavouriteObject` a given row's ★/☆ toggle targets, or `None` for
    /// rows that don't support favouriting (`Schema`/`Section`/`Column`/
    /// `Index`) and whenever there's no active connection id to stamp onto a
    /// new `FavouriteObject` (can't build one — and `is_favourite` would
    /// never match one from a different connection anyway). For
    /// `NodeId::Table`, the table/view distinction (`kind: "table"|"view"`)
    /// is looked up from `self.snapshot` since the node id alone doesn't
    /// carry it; a table that's since vanished from the snapshot (a rename/
    /// drop raced with the click) safely yields `None` rather than guessing.
    fn favourite_object_for(&self, id: &NodeId) -> Option<FavouriteObject> {
        let connection_id = self.active_connection_id.clone()?;
        let schema_opt = |s: &str| if s.is_empty() { None } else { Some(s.to_string()) };
        match id {
            NodeId::Table(schema, name) => {
                let kind = self
                    .snapshot
                    .as_ref()?
                    .tables
                    .iter()
                    .find(|t| &t.name == name && &schema_key_string(&t.schema) == schema)
                    .map(|t| match t.kind {
                        TableKind::Table => "table",
                        TableKind::View | TableKind::MaterializedView => "view",
                    })?;
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: kind.to_string() })
            }
            NodeId::Routine(schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: "routine".into() })
            }
            NodeId::Trigger(schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: "trigger".into() })
            }
            NodeId::Sequence(schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: "sequence".into() })
            }
            NodeId::Favourite(kind, schema, name) => {
                Some(FavouriteObject { connection_id, schema: schema_opt(schema), name: name.clone(), kind: kind.clone() })
            }
            _ => None,
        }
    }

    /// Called by `AppView::trigger_schema_fetch` right before dispatching
    /// `runner.fetch_schema` — shows the "Načítám…" row until the fetch
    /// resolves via `set_snapshot`/`set_error`.
    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();
    }

    /// `same_connection` (passed by the caller, `AppView::trigger_schema_fetch`,
    /// which knows whether this snapshot is a refresh of the connection
    /// already shown or a switch to a different one — see
    /// `conn_spec_key`/`schema_tree_connection_key` in `main.rs`) decides
    /// what happens to `expanded`/`filter`/`selected`:
    ///
    /// - Same connection (e.g. a ⟳ refresh): preserved. `NodeId`'s
    ///   path-based stability (see the module doc comment) means ids for
    ///   unchanged objects come back identical, so `prune_stale_ids` only
    ///   needs to drop the ones that no longer exist (a table/column that
    ///   was dropped since the last fetch) rather than resetting everything.
    /// - Different connection: reset entirely, since the new snapshot may
    ///   describe a completely different database — stale node ids (and a
    ///   stale filter hiding everything) would be actively misleading.
    pub fn set_snapshot(&mut self, snapshot: SchemaSnapshot, same_connection: bool, cx: &mut Context<Self>) {
        if same_connection {
            let (expanded, selected) = prune_stale_ids(&self.expanded, &self.selected, &snapshot);
            self.expanded = expanded;
            self.selected = selected;
        } else {
            self.expanded.clear();
            self.filter.clear();
            self.selected = None;
        }
        self.snapshot = Some(snapshot);
        self.loading = false;
        self.error = None;
        cx.notify();
    }

    /// G3 Task 5: read-only access to the current snapshot for the command
    /// palette's table/view source (`main.rs`'s `build_palette_items`) —
    /// `None` before any fetch has resolved, same as every other accessor
    /// here.
    pub fn snapshot(&self) -> Option<&SchemaSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.loading = false;
        self.error = Some(message);
        cx.notify();
    }

    /// Back to "Bez připojení" — used when there's no active connection to
    /// fetch a schema for (e.g. a `RefreshRequested` with nothing to
    /// refresh).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.loading = false;
        self.error = None;
        self.expanded.clear();
        self.filter.clear();
        self.selected = None;
        cx.notify();
    }

    fn toggle_expand(&mut self, id: &NodeId) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.clone());
        }
    }

    fn find_routine_ddl(&self, schema: &str, name: &str) -> Option<String> {
        self.snapshot
            .as_ref()?
            .routines
            .iter()
            .find(|r| r.name == name && schema_key_string(&r.schema) == schema)
            .and_then(|r| r.ddl.clone())
    }

    fn find_trigger_ddl(&self, schema: &str, name: &str) -> Option<String> {
        self.snapshot
            .as_ref()?
            .triggers
            .iter()
            .find(|t| t.name == name && schema_key_string(&t.schema) == schema)
            .and_then(|t| t.ddl.clone())
    }

    /// The currently-selected table/view's `TableInfo`, if `selected` points
    /// at one — used both to decide whether the header's "DDL" button is
    /// enabled and, on click, to build the DDL it opens (`handle_generate_ddl`).
    fn selected_table(&self) -> Option<&TableInfo> {
        let NodeId::Table(schema, name) = self.selected.as_ref()? else { return None };
        self.snapshot
            .as_ref()?
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

    /// Contract #4: double-click table/view -> `OpenPreview`; double-click
    /// routine/trigger -> `OpenDdl` (fallback text when no `ddl`);
    /// otherwise toggle expand.
    fn handle_double_click(&mut self, id: &NodeId, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        match id {
            NodeId::Table(schema, name) => {
                let schema = if schema.is_empty() { None } else { Some(schema.clone()) };
                cx.emit(TreeEvent::OpenPreview { schema, table: name.clone() });
            }
            NodeId::Routine(schema, name) => {
                let ddl = self.find_routine_ddl(schema, name).unwrap_or_else(|| DDL_FALLBACK.to_string());
                cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
            }
            NodeId::Trigger(schema, name) => {
                let ddl = self.find_trigger_ddl(schema, name).unwrap_or_else(|| DDL_FALLBACK.to_string());
                cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
            }
            // G3 Task 4: a favourites-section row uses the same double-click
            // semantics as its counterpart elsewhere in the tree — table/view
            // -> OpenPreview, routine/trigger -> OpenDdl. Sequences have no
            // double-click action anywhere in the tree, so that's a no-op
            // here too (falls through without emitting).
            NodeId::Favourite(kind, schema, name) => {
                let schema_opt = if schema.is_empty() { None } else { Some(schema.clone()) };
                match kind.as_str() {
                    "table" | "view" => {
                        cx.emit(TreeEvent::OpenPreview { schema: schema_opt, table: name.clone() });
                    }
                    "routine" => {
                        let ddl = self.find_routine_ddl(schema, name).unwrap_or_else(|| DDL_FALLBACK.to_string());
                        cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
                    }
                    "trigger" => {
                        let ddl = self.find_trigger_ddl(schema, name).unwrap_or_else(|| DDL_FALLBACK.to_string());
                        cx.emit(TreeEvent::OpenDdl { title: name.clone(), ddl });
                    }
                    _ => {}
                }
            }
            _ => self.toggle_expand(id),
        }
        cx.notify();
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

impl Render for SchemaTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self
            .snapshot
            .as_ref()
            .map(|s| {
                flatten(
                    s,
                    &self.expanded,
                    &self.filter,
                    &self.favourites,
                    self.active_connection_id.as_deref(),
                    self.admin_entry,
                )
            })
            .unwrap_or_default();

        let header_label =
            if self.filter.is_empty() { "Strom schémat".to_string() } else { format!("Strom schémat [{}]", self.filter) };
        // Task 7 contract #3: enabled only when the current selection is a
        // table/view (`selected_table`, not merely `self.selected.is_some()`
        // — e.g. a selected column or routine leaves it disabled).
        let ddl_enabled = self.selected_table().is_some();
        let ddl_color = if ddl_enabled { rgb(0xcdd6f4) } else { rgb(0x45475a) };

        let header = div()
            .h(px(28.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(rgb(0x181825))
            .text_color(rgb(0xcdd6f4))
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
            .bg(rgb(0x1e1e2e))
            .on_action(cx.listener(Self::on_tree_escape))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .child(header);

        if self.loading {
            root = root.child(div().px_2().py_1().text_color(rgb(0xa6adc8)).child("Načítám…"));
        } else if let Some(err) = &self.error {
            root = root.child(div().px_2().py_1().text_color(rgb(0xf38ba8)).child(format!("error: {err}")));
        } else if self.snapshot.is_none() {
            root = root.child(div().px_2().py_1().text_color(rgb(0x6c7086)).child("Bez připojení"));
        } else {
            root = root.child(
                uniform_list(
                    "schema-tree-rows",
                    rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let mut items = Vec::with_capacity(range.len());
                        for ix in range {
                            let (id, depth, label, expandable) = rows[ix].clone();
                            let is_expanded = this.expanded.contains(&id);
                            let is_selected = this.selected.as_ref() == Some(&id);
                            let chevron = if expandable {
                                if is_expanded { "▾" } else { "▸" }
                            } else {
                                " "
                            };

                            let click_id = id.clone();
                            let chevron_id = id.clone();

                            // G3 Task 4: a ★/☆ toggle, right-aligned (pushed
                            // there by the label's `flex_1()` below), for
                            // every favouritable row — `favourite_object_for`
                            // returns `None` for `Schema`/`Section`/`Column`/
                            // `Index` rows, which get no star at all.
                            let fav_obj = this.favourite_object_for(&id);
                            let is_fav = fav_obj.as_ref().is_some_and(|f| this.favourites.contains(f));
                            let star = fav_obj.map(|f| {
                                let (glyph, color) =
                                    if is_fav { ("★", rgb(0xf9e2af)) } else { ("☆", rgb(0x6c7086)) };
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

                            // G8 T6: a second icon-button, gated to
                            // `NodeId::Schema(_)` rows only — the schema-tree
                            // entry point for the ER diagram tab.
                            let diagram_icon = if let NodeId::Schema(s) = &id {
                                let schema_for_click =
                                    if s.is_empty() { None } else { Some(s.clone()) };
                                Some(
                                    div()
                                        .id(("tree-erd", ix))
                                        .px_1()
                                        .flex_shrink_0()
                                        .cursor_pointer()
                                        .text_color(rgb(0x89b4fa))
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

                            // G12 T4: a third icon-button, gated to
                            // `NodeId::Table(_, _)` rows AND hidden entirely
                            // (not merely disabled) when the tree is
                            // `read_only` — CURATION item 4(b)'s entry-gate
                            // half.
                            let csv_icon = if let NodeId::Table(schema, name) = &id {
                                if this.read_only {
                                    None
                                } else {
                                    let schema_for_click =
                                        if schema.is_empty() { None } else { Some(schema.clone()) };
                                    let table_for_click = name.clone();
                                    Some(
                                        div()
                                            .id(("tree-csv", ix))
                                            .px_1()
                                            .flex_shrink_0()
                                            .cursor_pointer()
                                            .text_color(rgb(0xa6e3a1))
                                            .child("⇪")
                                            .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                                                cx.stop_propagation();
                                                cx.emit(TreeEvent::ImportCsv {
                                                    schema: schema_for_click.clone(),
                                                    table: table_for_click.clone(),
                                                });
                                            })),
                                    )
                                }
                            } else {
                                None
                            };

                            // G10 T4 (design §2): the pinned "Správa serveru"
                            // row is neither expandable nor selectable like
                            // an ordinary tree node — greyed + an inline
                            // "(pouze pro čtení)" hint when `Disabled` (this
                            // codebase has no tooltip primitive elsewhere to
                            // reuse — see the module's admin_panel.rs
                            // sibling doc comment), and its click emits
                            // `TreeEvent::OpenAdmin` only when `Enabled`.
                            let is_admin_root = matches!(id, NodeId::AdminRoot);
                            let admin_disabled =
                                is_admin_root && this.admin_entry == AdminEntry::Disabled;
                            let admin_enabled =
                                is_admin_root && this.admin_entry == AdminEntry::Enabled;
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
                                .text_color(if admin_disabled { rgb(0x6c7086) } else { rgb(0xcdd6f4) })
                                .hover(|s| s.bg(rgb(0x313244)));
                            if is_selected {
                                row = row.bg(rgb(0x45475a));
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
                                            if !is_admin_root {
                                                this.toggle_expand(&chevron_id);
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(div().flex_1().overflow_hidden().child(label))
                                .on_click(cx.listener(move |this, ev: &ClickEvent, _window, cx| {
                                    if is_admin_root {
                                        if admin_enabled {
                                            cx.emit(TreeEvent::OpenAdmin);
                                        }
                                        return;
                                    }
                                    if ev.click_count() >= 2 {
                                        this.handle_double_click(&click_id, cx);
                                    } else {
                                        this.selected = Some(click_id.clone());
                                        cx.notify();
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
        let rows = flatten(&snap, &HashSet::new(), "", &[], None, AdminEntry::Hidden);
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
        let rows = flatten(&snap, &HashSet::new(), "", &[], None, AdminEntry::Hidden);
        // Only the two Schema headers show — nothing is expanded yet.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (NodeId::Schema("audit".into()), 0, "audit".into(), true));
        assert_eq!(rows[1], (NodeId::Schema("public".into()), 0, "public".into(), true));

        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Schema("public".into()));
        let rows = flatten(&snap, &expanded, "", &[], None, AdminEntry::Hidden);
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
        let rows = flatten(&snap, &HashSet::new(), "", &[], None, AdminEntry::Hidden);
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
        let rows = flatten(&snap, &HashSet::new(), "", &[], None, AdminEntry::Hidden);
        assert_eq!(rows, vec![(NodeId::Section("".into(), "Pohledy"), 0, "Pohledy (2)".into(), true)]);
    }

    #[test]
    fn expand_collapse_controls_child_visibility() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![col("id", "INTEGER")])],
            ..Default::default()
        };
        // Nothing expanded: only the section header.
        let rows = flatten(&snap, &HashSet::new(), "", &[], None, AdminEntry::Hidden);
        assert_eq!(rows.len(), 1);

        // Section expanded: table row appears, columns still hidden.
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::Section("".into(), "Tabulky"));
        let rows = flatten(&snap, &expanded, "", &[], None, AdminEntry::Hidden);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].0, NodeId::Table("".into(), "users".into()));

        // Table also expanded: column row appears too.
        expanded.insert(NodeId::Table("".into(), "users".into()));
        let rows = flatten(&snap, &expanded, "", &[], None, AdminEntry::Hidden);
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
        let rows = flatten(&snap, &HashSet::new(), "EMAIL", &[], None, AdminEntry::Hidden);
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
        let rows = flatten(&snap, &HashSet::new(), "users", &[], None, AdminEntry::Hidden);
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
        let rows = flatten(&snap, &expanded, "", &[], None, AdminEntry::Hidden);
        assert!(rows.iter().any(|(id, depth, label, _)| {
            *id == NodeId::Index("".into(), "users".into(), "users_pkey".into())
                && *depth == 1
                && label == "users.users_pkey"
        }));
    }

    #[test]
    fn empty_snapshot_flattens_to_no_rows() {
        let snap = SchemaSnapshot::default();
        assert!(flatten(&snap, &HashSet::new(), "", &[], None, AdminEntry::Hidden).is_empty());
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

    // --- G3 Task 4: favourites section ---

    fn fav(connection_id: &str, schema: Option<&str>, name: &str, kind: &str) -> FavouriteObject {
        FavouriteObject {
            connection_id: connection_id.into(),
            schema: schema.map(str::to_string),
            name: name.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn favourites_section_hidden_when_no_active_connection() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![])],
            ..Default::default()
        };
        let favourites = vec![fav("c1", None, "users", "table")];
        // No active connection id at all (e.g. the CLI-arg URL path) — the
        // section can't be built (nothing to stamp a new toggle with
        // either), so it's hidden even though `favourites` is non-empty.
        let rows = flatten(&snap, &HashSet::new(), "", &favourites, None, AdminEntry::Hidden);
        assert!(!rows.iter().any(|(id, ..)| matches!(id, NodeId::FavouriteSection)));
    }

    #[test]
    fn favourites_section_hidden_when_none_belong_to_active_connection() {
        let snap = SchemaSnapshot {
            tables: vec![table(None, "users", TableKind::Table, vec![])],
            ..Default::default()
        };
        let favourites = vec![fav("other-conn", None, "users", "table")];
        let rows = flatten(&snap, &HashSet::new(), "", &favourites, Some("c1"), AdminEntry::Hidden);
        assert!(!rows.iter().any(|(id, ..)| matches!(id, NodeId::FavouriteSection)));
    }

    #[test]
    fn favourites_section_renders_first_before_schemas() {
        let snap = SchemaSnapshot {
            tables: vec![
                table(Some("public"), "t1", TableKind::Table, vec![]),
                table(Some("audit"), "t2", TableKind::Table, vec![]),
            ],
            ..Default::default()
        };
        let favourites = vec![fav("c1", Some("public"), "t1", "table")];
        let rows = flatten(&snap, &HashSet::new(), "", &favourites, Some("c1"), AdminEntry::Hidden);
        assert_eq!(rows[0].0, NodeId::FavouriteSection);
        assert_eq!(rows[0].2, "Oblíbené (1)");
        // The Schema headers still follow, unaffected.
        assert!(rows.iter().any(|(id, ..)| *id == NodeId::Schema("audit".into())));
        assert!(rows.iter().any(|(id, ..)| *id == NodeId::Schema("public".into())));
    }

    #[test]
    fn favourites_section_only_shows_active_connection_items_cross_schema() {
        let snap = SchemaSnapshot::default();
        let favourites = vec![
            fav("c1", Some("public"), "t1", "table"),
            fav("c1", Some("audit"), "f1", "routine"),
            fav("c2", Some("public"), "other", "table"),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::FavouriteSection);
        let rows = flatten(&snap, &expanded, "", &favourites, Some("c1"), AdminEntry::Hidden);
        assert_eq!(rows[0], (NodeId::FavouriteSection, 0, "Oblíbené (2)".into(), true));
        let items: Vec<&NodeId> = rows.iter().skip(1).map(|(id, ..)| id).collect();
        assert_eq!(
            items,
            vec![
                &NodeId::Favourite("table".into(), "public".into(), "t1".into()),
                &NodeId::Favourite("routine".into(), "audit".into(), "f1".into()),
            ]
        );
        // "c2"'s favourite never shows, and no dangling reference to it.
        assert!(!rows.iter().any(|(id, ..)| *id == NodeId::Favourite("table".into(), "public".into(), "other".into())));
    }

    #[test]
    fn favourites_section_labels_schema_dot_name_or_bare_name() {
        let snap = SchemaSnapshot::default();
        let favourites = vec![fav("c1", Some("public"), "t1", "table"), fav("c1", None, "seq1", "sequence")];
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::FavouriteSection);
        let rows = flatten(&snap, &expanded, "", &favourites, Some("c1"), AdminEntry::Hidden);
        let labels: Vec<&str> = rows.iter().skip(1).map(|(_, _, l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["public.t1", "seq1"]);
    }

    #[test]
    fn favourites_section_stays_collapsed_until_expanded() {
        let snap = SchemaSnapshot::default();
        let favourites = vec![fav("c1", Some("public"), "t1", "table")];
        let rows = flatten(&snap, &HashSet::new(), "", &favourites, Some("c1"), AdminEntry::Hidden);
        // Header only — not expanded, so the item row is hidden.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, NodeId::FavouriteSection);
    }

    /// The favourites section's `NodeId::Favourite(kind, schema, name)`
    /// carries whatever `kind` the `FavouriteObject` was stored under —
    /// this is exactly the data `handle_double_click`'s `NodeId::Favourite`
    /// arm switches on to decide OpenPreview (table/view) vs OpenDdl
    /// (routine/trigger) vs no-op (sequence), so asserting it round-trips
    /// unchanged through `flatten` is what makes that dispatch correct.
    #[test]
    fn favourites_section_node_ids_carry_kind_for_double_click_dispatch() {
        let snap = SchemaSnapshot::default();
        let favourites = vec![
            fav("c1", Some("s"), "a_table", "table"),
            fav("c1", Some("s"), "a_view", "view"),
            fav("c1", Some("s"), "a_routine", "routine"),
            fav("c1", Some("s"), "a_trigger", "trigger"),
            fav("c1", Some("s"), "a_sequence", "sequence"),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::FavouriteSection);
        let rows = flatten(&snap, &expanded, "", &favourites, Some("c1"), AdminEntry::Hidden);
        let ids: Vec<&NodeId> = rows.iter().skip(1).map(|(id, ..)| id).collect();
        assert_eq!(
            ids,
            vec![
                &NodeId::Favourite("table".into(), "s".into(), "a_table".into()),
                &NodeId::Favourite("view".into(), "s".into(), "a_view".into()),
                &NodeId::Favourite("routine".into(), "s".into(), "a_routine".into()),
                &NodeId::Favourite("trigger".into(), "s".into(), "a_trigger".into()),
                &NodeId::Favourite("sequence".into(), "s".into(), "a_sequence".into()),
            ]
        );
    }

    // G10 T4: the pinned "Správa serveru" row renders first (even ahead of
    // "Oblíbené") whenever `AdminEntry` isn't `Hidden`, and never appears
    // at all when it is.
    #[test]
    fn admin_root_renders_first_when_not_hidden_and_never_when_hidden() {
        let snapshot = SchemaSnapshot::default();
        let expanded = HashSet::new();
        let out = flatten(&snapshot, &expanded, "", &[], None, AdminEntry::Enabled);
        assert_eq!(
            out.first().map(|(id, depth, label, _)| (id.clone(), *depth, label.clone())),
            Some((NodeId::AdminRoot, 0, "Správa serveru".to_string()))
        );
        let out = flatten(&snapshot, &expanded, "", &[], None, AdminEntry::Disabled);
        assert!(matches!(out.first(), Some((NodeId::AdminRoot, ..))));
        let out = flatten(&snapshot, &expanded, "", &[], None, AdminEntry::Hidden);
        assert!(out.iter().all(|(id, ..)| *id != NodeId::AdminRoot));
    }
}
