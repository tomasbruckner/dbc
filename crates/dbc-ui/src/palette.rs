// G3 Task 5: Ctrl+K command palette.
//
// Layout of this file:
//   1. `fuzzy_score` — pure, GPUI-free case-insensitive subsequence scorer.
//   2. `PaletteItem`/`PaletteAction` — the palette's result rows and the
//      fixed actions it can dispatch.
//   3. Source structs (`TableSource`/`HistorySource`/`ConnectionSource`) +
//      `rank_items` — pure assembly/scoring of everything the palette shows,
//      unit-tested directly below with no GPUI dependency at all.
//   4. `display_label` — the "T "/"H "/"C "/"A " prefixed row text
//      `main.rs`'s render helper uses.
//   5. `bind_keys` + the palette's own scoped actions (Up/Down/Confirm/Close,
//      key context "Palette") — same pattern as `schema_tree::bind_keys`/
//      `connections_ui::bind_keys`. `main.rs` owns `OpenPalette` itself
//      (an app-level action alongside `RunQuery`/`ToggleTree`/etc.), since
//      Ctrl+K needs to fire regardless of what currently has focus.
//
// `main.rs` owns all GPUI wiring (overlay render, `PaletteState`, execution
// routing through the existing `run_query_with`/`switch_to_connection`/
// `open_connection_dialog`/history paths) — this file is intentionally
// GPUI-App-free except for the `actions!`/`bind_keys` plumbing in section 5.

use gpui::{actions, App, KeyBinding};

/// Case-insensitive subsequence match: every character of `query` must
/// appear in `target`, in order (not necessarily contiguous), or this
/// returns `None`. When it matches, higher is better:
/// - each matched character contributes a base point,
/// - a character matched immediately after the previous match (a
///   consecutive run) contributes an extra bonus,
/// - a character matched at a word boundary (start of `target`, or preceded
///   by a non-alphanumeric character) contributes another bonus,
/// - the whole per-character sum is scaled up so a shorter `target` — all
///   else equal — still nudges the final score higher (ties broken toward
///   the more specific/shorter match), without a length difference ever
///   overriding a genuine bonus difference.
pub fn fuzzy_score(query: &str, target: &str) -> Option<i64> {
    const CONSECUTIVE_BONUS: i64 = 15;
    const WORD_BOUNDARY_BONUS: i64 = 10;
    const SCALE: i64 = 1000;

    // G3 final-review fix (F1): `target_lower` must stay index-aligned with
    // `target_chars` 1:1, since `idx` computed against one is used to index
    // the other below. `target.to_lowercase()` (whole-string) can *expand*
    // for some characters (e.g. 'İ' U+0130 → "i\u{307}", 2 chars), which
    // desyncs the two Vecs and panics on out-of-bounds indexing. Lowering
    // each `char` independently and keeping only its first lowered char
    // (falling back to the original on an empty iterator, which never
    // happens for `char::to_lowercase`) guarantees the same length by
    // construction — a minor ranking-quality loss for chars whose lowercase
    // expansion carries real information, acceptable for a fuzzy scorer.
    let target_chars: Vec<char> = target.chars().collect();
    let target_lower: Vec<char> =
        target_chars.iter().map(|&c| c.to_lowercase().next().unwrap_or(c)).collect();

    if query.is_empty() {
        return Some(-(target_chars.len() as i64));
    }

    let mut search_from = 0usize;
    let mut score = 0i64;
    let mut prev_match: Option<usize> = None;

    for qc in query.to_lowercase().chars() {
        let idx = target_lower[search_from..].iter().position(|&c| c == qc).map(|i| i + search_from)?;

        let mut char_score = 1i64;
        if idx > 0 && prev_match == Some(idx - 1) {
            char_score += CONSECUTIVE_BONUS;
        }
        let is_boundary = idx == 0 || !target_chars[idx - 1].is_alphanumeric();
        if is_boundary {
            char_score += WORD_BOUNDARY_BONUS;
        }

        score += char_score;
        prev_match = Some(idx);
        search_from = idx + 1;
    }

    Some(score * SCALE - target_chars.len() as i64)
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaletteItem {
    /// → open a preview tab, same SQL/tab-replace logic as
    /// `TreeEvent::OpenPreview`.
    Table { schema: Option<String>, name: String },
    /// → load into the SQL editor + focus it (never runs it).
    HistoryEntry { id: i64, sql: String },
    /// → `switch_to_connection`.
    Connection { id: String, name: String },
    /// → dispatch the respective existing `AppView` method.
    Action { label: String, action: PaletteAction },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteAction {
    RunQuery,
    ToggleTree,
    ToggleHistory,
    NewConnection,
    RefreshSchema,
    OpenMonitor,
    /// G8 T6: opens the ER diagram tab for the single unambiguous schema in
    /// the current snapshot (`AppView::resolve_er_diagram_schema`), or
    /// refuses with a Czech status pointing at the schema-tree icon.
    ShowErDiagram,
    /// Forgets every cached schema snapshot. The escape hatch for „the
    /// tree is showing a table that no longer exists" — the per-database ⟳
    /// already refreshes one slot, this is the whole-app version.
    ClearSchemaCache,
    /// Opens the tail of the diagnostic log in a text tab. The log lives
    /// in the profile directory, which most users never open — an app that
    /// keeps a log the user cannot find keeps it for nobody.
    ShowLog,
    /// G7 T6: opens `ModalState::CompareDialog` (design §3's entry point).
    OpenCompare,
    /// G11 T6: opens `ModalState::BackupRestore` in `BackupKind::Backup`
    /// mode for the currently active connection.
    BackupDatabase,
    /// G11 T6: opens `ModalState::BackupRestore` in `BackupKind::Restore`
    /// mode for the currently active connection. Listed (discoverable)
    /// whenever a connection is active regardless of its read-only flag —
    /// the read-only refusal happens at CLICK time (design's own scope
    /// reduction, see this plan's T6 Grounding: no disabled-row rendering
    /// precedent exists in this file today).
    RestoreDatabase,
    /// G12 T3: opens the file-picker + pre-scan + confirm-modal flow for a
    /// single `.sql` script (`AppView::start_script_pick(false, ..)`).
    RunSqlFile,
    /// G12 T3: same flow, folder mode — non-recursive `*.sql` listing
    /// (`AppView::start_script_pick(true, ..)`).
    RunSqlFolder,
    /// Part S §8 (workspace T8): the palette gains exactly ONE scripts
    /// item — per-script palette rows would require the palette to hold
    /// the scan, which is a follow-up candidate, not this phase. Dispatches
    /// `AppView::on_save_script`, i.e. the SAME entry point as Ctrl+S and
    /// the caption strip's „Uložit": bound => save, unbound => save-as.
    /// Unconditional, like „Spustit SQL soubor…" — a palette has no
    /// disabled-row idiom, and the unbound case is a real action, not a
    /// no-op.
    SaveScript,
    /// G10 T4: opens (or re-focuses) the "Správa serveru" admin tab
    /// (`AppView::open_admin_tab`) — only ever offered when
    /// `admin_panel::admin_entry_state` is `Enabled` (see `fixed_actions`;
    /// `Hidden`/`Disabled` both simply omit the row — a palette has no
    /// disabled-row idiom, the schema-tree row is where `Disabled` is
    /// explained).
    OpenServerAdmin,
    /// G14 T10: toggles dark<->light directly (no submenu) — unconditional,
    /// same posture as every other always-listed fixed action (design §1.5).
    ToggleTheme,
    /// G14 T11: opens `ModalState::ChartPicker` — the axis picker for the
    /// active Grid tab's `ResultBuffer` snapshot.
    OpenChart,
    /// App-wide master password UX design §2: the proactive unlock — opens
    /// `MasterPasswordPrompt` with `PendingAfterUnlock::Nothing` (no
    /// interrupted action to resume). Listed only while a vault file exists
    /// and is currently locked (`fixed_actions`'s `vault_unlockable`).
    UnlockVault,
    /// App-wide master password UX design §3: `self.vault = None` — the
    /// `Drop` impl zeroizes the derived key + decrypted secrets. Listed
    /// only while the vault is currently unlocked (`fixed_actions`'s
    /// `vault_lockable`).
    LockVault,
}

/// One table/view from the current schema snapshot, plus whether it's
/// favourited (drives the ranking bonus — brief: palette ranking).
pub struct TableSource {
    pub schema: Option<String>,
    pub name: String,
    pub favourite: bool,
}

/// One history entry, already resolved via `HistoryDb::search` by the
/// caller (main.rs) — this module never touches the DB itself.
pub struct HistorySource {
    pub id: i64,
    pub sql: String,
}

pub struct ConnectionSource {
    pub id: String,
    pub name: String,
    pub favourite: bool,
}

/// Favourite objects/connections rank first among otherwise-equal matches
/// (brief: palette ranking bonus). Applied on top of `fuzzy_score`'s output,
/// never in place of it — a poor match with the bonus still loses to a
/// strong match without it.
const FAVOURITE_BONUS: i64 = 1000;

/// The fixed action rows, in display order, with their Czech labels (brief
/// contract #3). `monitor_available` gates the monitor entry per the ACTIVE
/// connection's engine (design §7): absent entirely — not disabled-but-
/// visible — when the engine has no monitor (showing it for an engine
/// without a monitor would just surface a confusing driver-missing error).
/// `connection_active` (G11 T6, design §4c): the two backup/restore entries
/// are appended LAST, and only "shown only when a connection is currently
/// active" (design's own words) — an inactive/no-connection state hides
/// them entirely rather than showing them disabled (no disabled-row
/// rendering precedent exists anywhere in this file — see T6's Grounding).
/// `chart_available` (G14 T11, design §2.1): same absent-not-disabled
/// posture as `monitor_available` — listed only while the active tab is a
/// Grid with results to chart. Placed BEFORE the backup/restore block so
/// those two stay the literal last two rows (existing test invariant).
/// `vault_unlockable`/`vault_lockable` (app-wide master password UX design
/// §2/§3/§7): "Odemknout trezor" listed only while a vault file exists AND
/// is currently locked; "Zamknout trezor" listed only while unlocked —
/// mutually exclusive in practice (the vault is never both), each gated
/// independently rather than as one three-state action so a future state
/// (e.g. "no vault file yet") simply hides both without a match arm to
/// update. Placed BEFORE the backup/restore block, same reason as
/// `chart_available` — those two stay the literal last two rows.
pub fn fixed_actions(
    monitor_available: bool,
    admin: crate::admin_panel::AdminEntry,
    connection_active: bool,
    chart_available: bool,
    vault_unlockable: bool,
    vault_lockable: bool,
) -> Vec<(String, PaletteAction)> {
    let mut actions = vec![
        ("Spustit dotaz".to_string(), PaletteAction::RunQuery),
        ("Přepnout strom".to_string(), PaletteAction::ToggleTree),
        ("Přepnout historii".to_string(), PaletteAction::ToggleHistory),
        ("Nové spojení…".to_string(), PaletteAction::NewConnection),
        ("Obnovit schéma".to_string(), PaletteAction::RefreshSchema),
        ("ER diagram".to_string(), PaletteAction::ShowErDiagram),
        ("Porovnat databáze…".to_string(), PaletteAction::OpenCompare),
        ("Spustit SQL soubor…".to_string(), PaletteAction::RunSqlFile),
        ("Spustit SQL složku…".to_string(), PaletteAction::RunSqlFolder),
        // Part S §8: kept among the LEADING unconditional rows, never
        // appended — `backup_restore_actions_present_and_last_when_
        // connection_active` pins the last two rows.
        ("Uložit skript".to_string(), PaletteAction::SaveScript),
        // G14 T10: unconditional — always listed, kept ahead of the
        // conditional monitor/backup/restore rows below so
        // `backup_restore_actions_present_and_last_when_connection_active`'s
        // "last two rows" assumption keeps holding.
        ("Přepnout motiv".to_string(), PaletteAction::ToggleTheme),
        ("Otevřít log".to_string(), PaletteAction::ShowLog),
        ("Vymazat mezipaměť schémat".to_string(), PaletteAction::ClearSchemaCache),
    ];
    if monitor_available {
        actions.push(("Monitor serveru".to_string(), PaletteAction::OpenMonitor));
    }
    // G10 T4 (design §2, resolved design ambiguity 4): Hidden AND Disabled
    // both omit the row — only Enabled shows it.
    if admin == crate::admin_panel::AdminEntry::Enabled {
        actions.push(("Správa serveru".to_string(), PaletteAction::OpenServerAdmin));
    }
    if chart_available {
        actions.push(("Graf z výsledku".to_string(), PaletteAction::OpenChart));
    }
    if vault_unlockable {
        actions.push(("Odemknout trezor".to_string(), PaletteAction::UnlockVault));
    }
    if vault_lockable {
        actions.push(("Zamknout trezor".to_string(), PaletteAction::LockVault));
    }
    if connection_active {
        actions.push(("Zálohovat databázi…".to_string(), PaletteAction::BackupDatabase));
        actions.push(("Obnovit databázi ze zálohy…".to_string(), PaletteAction::RestoreDatabase));
    }
    actions
}

fn table_search_text(t: &TableSource) -> String {
    match &t.schema {
        Some(s) if !s.is_empty() => format!("{s}.{}", t.name),
        _ => t.name.clone(),
    }
}

/// Pure assembly + scoring of every palette source into the final ranked
/// row list, capped to `cap` rows.
///
/// Empty `query` (brief contract #3): a fixed category order — favourite
/// tables/views first (alphabetical), then `history` as given (the caller
/// already asked `HistoryDb::search` for the top-N recent), then
/// `connections` as given, then the fixed actions — rather than
/// `fuzzy_score`-based ordering (an empty query trivially "matches"
/// everything, so scoring it would be meaningless).
///
/// Non-empty `query`: every source is scored via `fuzzy_score` against its
/// searchable text (table: `schema.name` or just `name`; history: the raw
/// SQL; connection: its name; action: its Czech label); non-matches
/// (`None`) are dropped, matches get the favourite bonus where applicable,
/// and the whole set is sorted by score descending.
pub fn rank_items(
    query: &str,
    tables: &[TableSource],
    history: &[HistorySource],
    connections: &[ConnectionSource],
    monitor_available: bool,
    admin: crate::admin_panel::AdminEntry,
    cap: usize,
    connection_active: bool,
    chart_available: bool,
    vault_unlockable: bool,
    vault_lockable: bool,
) -> Vec<PaletteItem> {
    if query.trim().is_empty() {
        let mut out = Vec::new();

        let mut favourite_tables: Vec<&TableSource> = tables.iter().filter(|t| t.favourite).collect();
        favourite_tables.sort_by(|a, b| a.name.cmp(&b.name));
        for t in favourite_tables {
            out.push(PaletteItem::Table { schema: t.schema.clone(), name: t.name.clone() });
        }
        for h in history {
            out.push(PaletteItem::HistoryEntry { id: h.id, sql: h.sql.clone() });
        }
        for c in connections {
            out.push(PaletteItem::Connection { id: c.id.clone(), name: c.name.clone() });
        }
        for (label, action) in fixed_actions(
            monitor_available,
            admin,
            connection_active,
            chart_available,
            vault_unlockable,
            vault_lockable,
        ) {
            out.push(PaletteItem::Action { label, action });
        }

        out.truncate(cap);
        return out;
    }

    let mut scored: Vec<(i64, PaletteItem)> = Vec::new();

    for t in tables {
        if let Some(mut score) = fuzzy_score(query, &table_search_text(t)) {
            if t.favourite {
                score += FAVOURITE_BONUS;
            }
            scored.push((score, PaletteItem::Table { schema: t.schema.clone(), name: t.name.clone() }));
        }
    }
    for h in history {
        if let Some(score) = fuzzy_score(query, &h.sql) {
            scored.push((score, PaletteItem::HistoryEntry { id: h.id, sql: h.sql.clone() }));
        }
    }
    for c in connections {
        if let Some(mut score) = fuzzy_score(query, &c.name) {
            if c.favourite {
                score += FAVOURITE_BONUS;
            }
            scored.push((score, PaletteItem::Connection { id: c.id.clone(), name: c.name.clone() }));
        }
    }
    for (label, action) in fixed_actions(
        monitor_available,
        admin,
        connection_active,
        chart_available,
        vault_unlockable,
        vault_lockable,
    ) {
        if let Some(score) = fuzzy_score(query, &label) {
            scored.push((score, PaletteItem::Action { label, action }));
        }
    }

    // `sort_by` (stable) rather than `sort_unstable_by`: ties preserve the
    // tables → history → connections → actions assembly order above, which
    // gives non-empty-query results the same category precedence as the
    // empty-query fixed order when scores happen to tie.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(cap);
    scored.into_iter().map(|(_, item)| item).collect()
}

/// The "T "/"H "/"C "/"A " prefixed row text (brief contract #3) `main.rs`
/// renders for each item.
pub fn display_label(item: &PaletteItem) -> String {
    match item {
        PaletteItem::Table { schema, name } => {
            let full = match schema {
                Some(s) if !s.is_empty() => format!("{s}.{name}"),
                _ => name.clone(),
            };
            format!("T {full}")
        }
        PaletteItem::HistoryEntry { sql, .. } => {
            format!("H {}", crate::history_panel::collapse_sql(sql, 48))
        }
        PaletteItem::Connection { name, .. } => format!("C {name}"),
        PaletteItem::Action { label, .. } => format!("A {label}"),
    }
}

// ---------------------------------------------------------------------
// GPUI plumbing: the palette's own scoped actions (Up/Down/Confirm/Close),
// key-context "Palette" — same "scoped binding wins over the app-level
// unscoped one" pattern as `TextField`/`SchemaTree` (see their module doc
// comments). `main.rs` attaches these via `cx.listener` on the palette
// overlay's root div, which also carries `.key_context("Palette")` so the
// palette's `TextField` child (its own `key_context("TextField")` has no
// Up/Down/Enter/Escape bindings of its own) still resolves them.
// ---------------------------------------------------------------------

actions!(palette, [PaletteUp, PaletteDown, PaletteConfirm, PaletteClose]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", PaletteUp, Some("Palette")),
        KeyBinding::new("down", PaletteDown, Some("Palette")),
        KeyBinding::new("enter", PaletteConfirm, Some("Palette")),
        KeyBinding::new("escape", PaletteClose, Some("Palette")),
    ]);
}

#[cfg(test)]
mod fuzzy_score_tests {
    use super::*;

    #[test]
    fn subsequence_miss_returns_none() {
        assert_eq!(fuzzy_score("xyz", "abc"), None);
        assert_eq!(fuzzy_score("orders", "products"), None);
    }

    #[test]
    fn subsequence_hit_returns_some() {
        assert!(fuzzy_score("odr", "orders").is_some());
        assert!(fuzzy_score("ORD", "orders").is_some()); // case-insensitive
    }

    #[test]
    fn consecutive_run_beats_scattered_match() {
        let consecutive = fuzzy_score("ab", "xabx").unwrap();
        let scattered = fuzzy_score("ab", "xaxbx").unwrap();
        assert!(consecutive > scattered, "consecutive={consecutive} scattered={scattered}");
    }

    #[test]
    fn word_boundary_hit_beats_mid_word_hit() {
        let boundary = fuzzy_score("b", "foo_bar").unwrap(); // 'b' right after '_'
        let mid_word = fuzzy_score("b", "foobar").unwrap(); // 'b' right after 'o'
        assert!(boundary > mid_word, "boundary={boundary} mid_word={mid_word}");
    }

    #[test]
    fn shorter_target_wins_ties() {
        let short = fuzzy_score("cat", "cat").unwrap();
        let long = fuzzy_score("cat", "cats_table").unwrap();
        assert!(short > long, "short={short} long={long}");
    }

    // G3 final-review regression (F1): 'İ' (U+0130, Turkish capital dotted
    // I) lowercases to a 2-char sequence ("i\u{307}"), which used to desync
    // `target_lower`'s index space from `target_chars`'s and panic on
    // out-of-bounds indexing. Neither call should panic, and both should
    // still find a genuine subsequence match.
    #[test]
    fn lowercase_expanding_char_in_target_does_not_panic() {
        // Empirical repro from the final review: target "İİİa" has 4
        // original chars but 'İ' (U+0130) lowercases to 2 chars each, so a
        // whole-string `target.to_lowercase()` desyncs to 7 chars — a query
        // matching near the end used to index `target_chars` out of bounds.
        // Query "a" matches the trailing 'a' — must not panic, must match.
        assert!(fuzzy_score("a", "İİİa").is_some());
        // Query "İ" itself lowercases (query-side, unrelated to the target
        // fix) to a 2-char sequence whose second char has no counterpart in
        // the per-char-lowered target — must not panic, even though it
        // correctly fails to match.
        assert_eq!(fuzzy_score("İ", "İİİa"), None);
    }

    #[test]
    fn lowercase_expanding_char_in_query_does_not_panic() {
        // Query-side lowering was never the source of the panic (its index
        // space is never reused to index back into anything), but it's
        // cheap to cover it too: this must not panic regardless of the
        // Some/None outcome.
        let _ = fuzzy_score("İ", "istanbul");
        assert_eq!(fuzzy_score("İ", "xyz"), None);
    }
}

#[cfg(test)]
mod rank_items_tests {
    use super::*;
    use crate::admin_panel::AdminEntry;

    fn table(schema: Option<&str>, name: &str, favourite: bool) -> TableSource {
        TableSource { schema: schema.map(str::to_string), name: name.to_string(), favourite }
    }

    fn history(id: i64, sql: &str) -> HistorySource {
        HistorySource { id, sql: sql.to_string() }
    }

    fn conn(id: &str, name: &str, favourite: bool) -> ConnectionSource {
        ConnectionSource { id: id.to_string(), name: name.to_string(), favourite }
    }

    #[test]
    fn empty_query_orders_favourites_then_history_then_connections_then_actions() {
        let tables =
            vec![table(None, "zzz_fav", true), table(None, "aaa_normal", false), table(None, "aaa_fav", true)];
        let history = vec![history(1, "select 1"), history(2, "select 2")];
        let connections = vec![conn("c1", "prod", false)];

        let items =
            rank_items("", &tables, &history, &connections, false, AdminEntry::Hidden, 30, false, false, false, false);

        // Favourites (alphabetical) first, then history (as given), then
        // connections, then the fixed actions (5 base + ER diagram +
        // G7's compare + G12's two script actions = 9,
        // monitor_available=false here).
        assert_eq!(
            items[0],
            PaletteItem::Table { schema: None, name: "aaa_fav".into() }
        );
        assert_eq!(items[1], PaletteItem::Table { schema: None, name: "zzz_fav".into() });
        assert_eq!(items[2], PaletteItem::HistoryEntry { id: 1, sql: "select 1".into() });
        assert_eq!(items[3], PaletteItem::HistoryEntry { id: 2, sql: "select 2".into() });
        assert_eq!(items[4], PaletteItem::Connection { id: "c1".into(), name: "prod".into() });
        assert!(matches!(items[5], PaletteItem::Action { .. }));
        // 5 base actions + G8 T6's "ER diagram" (`ShowErDiagram`) + G7's
        // "Porovnat databáze…" (`OpenCompare`) + G12 T3's "Spustit SQL
        // soubor…"/"Spustit SQL složku…" + workspace T8's "Uložit skript"
        // (`SaveScript`) + G14 T10's "Přepnout motiv" (`ToggleTheme`) +
        // the log viewer (`ShowLog`) — all unconditional, unlike
        // `OpenMonitor` which is engine-gated (monitor_available=false).
        assert_eq!(items.len(), 2 + 2 + 1 + 13);
    }

    #[test]
    fn empty_query_is_capped() {
        // Only favourites contribute to the "tables" category on an empty
        // query (brief contract #3 lists favourites/history/connections/
        // actions — not the whole unfiltered table list).
        let tables: Vec<TableSource> = (0..50).map(|i| table(None, &format!("t{i}"), true)).collect();
        let items = rank_items("", &tables, &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert_eq!(items.len(), 30);
    }

    #[test]
    fn non_matching_query_drops_items_that_dont_subsequence_match() {
        let tables = vec![table(None, "orders", false)];
        let items = rank_items("zzz", &tables, &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items.is_empty());
    }

    #[test]
    fn favourite_ranks_first_among_otherwise_equal_matches() {
        // Same-length schema prefixes ("aaaaa"/"bbbbb") + identical table
        // name -> identical base fuzzy_score (same match positions, same
        // target length) -> the favourite bonus is the only thing that can
        // separate them.
        let tables = vec![table(Some("aaaaa"), "orders", false), table(Some("bbbbb"), "orders", true)];
        assert_eq!(
            fuzzy_score("orders", &table_search_text(&tables[0])),
            fuzzy_score("orders", &table_search_text(&tables[1])),
            "test setup must produce a genuine base-score tie"
        );
        let items = rank_items("orders", &tables, &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert_eq!(
            items[0],
            PaletteItem::Table { schema: Some("bbbbb".into()), name: "orders".into() }
        );
    }

    #[test]
    fn better_fuzzy_match_beats_a_favourite_with_a_much_weaker_match() {
        // A long, scattered, no-boundary, non-consecutive match on a
        // favourite must not beat a tight consecutive+boundary match on a
        // non-favourite — the +1000 bonus tips a genuine tie, it doesn't
        // override a real quality gap.
        let mut weak_match = String::from("z");
        for (i, c) in "orders".chars().enumerate() {
            if i > 0 {
                weak_match.push_str("zzzz"); // breaks the consecutive-run bonus
            }
            weak_match.push(c); // each hit is preceded by 'z' (alnum): no boundary bonus either
        }
        weak_match.push_str(&"z".repeat(30)); // long target: length penalty on top

        let tables = vec![table(None, &weak_match, true), table(None, "orders", false)];
        let items = rank_items("orders", &tables, &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert_eq!(items[0], PaletteItem::Table { schema: None, name: "orders".into() });
    }

    #[test]
    fn monitor_entry_present_only_when_available() {
        let items = rank_items("", &[], &[], &[], true, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items
            .iter()
            .any(|i| matches!(i, PaletteItem::Action { action: PaletteAction::OpenMonitor, .. })));
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items
            .iter()
            .all(|i| !matches!(i, PaletteItem::Action { action: PaletteAction::OpenMonitor, .. })));
    }

    // G10 T4 (design ambiguity 4): the admin action row appears ONLY when
    // Enabled — Hidden AND Disabled both omit it, since the palette has no
    // disabled-row idiom (the schema-tree row explains the disabled state).
    #[test]
    fn admin_entry_present_only_when_enabled() {
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Enabled, 30, false, false, false, false);
        assert!(items
            .iter()
            .any(|i| matches!(i, PaletteItem::Action { action: PaletteAction::OpenServerAdmin, .. })));
        for admin in [AdminEntry::Hidden, AdminEntry::Disabled] {
            let items = rank_items("", &[], &[], &[], false, admin, 30, false, false, false, false);
            assert!(items
                .iter()
                .all(|i| !matches!(i, PaletteItem::Action { action: PaletteAction::OpenServerAdmin, .. })));
        }
    }

    #[test]
    fn results_are_capped_at_30() {
        let tables: Vec<TableSource> = (0..50).map(|i| table(None, &format!("orders_{i}"), false)).collect();
        let items = rank_items("orders", &tables, &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert_eq!(items.len(), 30);
    }

    // --- G11 T6: backup/restore actions gated on connection_active ---
    #[test]
    fn backup_restore_actions_hidden_without_active_connection() {
        let actions = fixed_actions(false, AdminEntry::Hidden, false, false, false, false);
        assert!(!actions.iter().any(|(_, a)| matches!(a, PaletteAction::BackupDatabase)));
        assert!(!actions.iter().any(|(_, a)| matches!(a, PaletteAction::RestoreDatabase)));

        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items
            .iter()
            .all(|i| !matches!(i, PaletteItem::Action { action: PaletteAction::BackupDatabase, .. })
                && !matches!(i, PaletteItem::Action { action: PaletteAction::RestoreDatabase, .. })));
    }

    #[test]
    fn backup_restore_actions_present_and_last_when_connection_active() {
        let actions = fixed_actions(false, AdminEntry::Hidden, true, false, false, false);
        assert_eq!(actions.last().unwrap().1, PaletteAction::RestoreDatabase);
        assert_eq!(actions[actions.len() - 2].1, PaletteAction::BackupDatabase);

        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, true, false, false, false);
        assert!(items
            .iter()
            .any(|i| matches!(i, PaletteItem::Action { action: PaletteAction::BackupDatabase, .. })));
        assert!(items
            .iter()
            .any(|i| matches!(i, PaletteItem::Action { action: PaletteAction::RestoreDatabase, .. })));
    }

    /// Part S §8 (workspace T8): „Uložit skript" is unconditional — it is
    /// a real action in BOTH binding states (save, or save-as) — and it
    /// sits among the LEADING rows, never at the end, so
    /// `backup_restore_actions_present_and_last_when_connection_active`'s
    /// "last two rows" assumption keeps holding.
    #[test]
    fn save_script_is_unconditional_and_never_one_of_the_last_two_rows() {
        for connection_active in [false, true] {
            let actions =
                fixed_actions(false, AdminEntry::Hidden, connection_active, false, false, false);
            let idx = actions
                .iter()
                .position(|(_, a)| *a == PaletteAction::SaveScript)
                .expect("the save-script row must always be listed");
            assert_eq!(actions[idx].0, "Uložit skript");
            // Wedged INSIDE the leading unconditional block — directly
            // after „Spustit SQL složku…" (per the plan) and still ahead of
            // „Přepnout motiv", which is the block's own last row.
            assert_eq!(actions[idx - 1].1, PaletteAction::RunSqlFolder);
            assert_eq!(actions[idx + 1].1, PaletteAction::ToggleTheme);
        }
        // …so the conditional trailing rows keep their pinned positions.
        let with_conn = fixed_actions(false, AdminEntry::Hidden, true, false, false, false);
        assert_eq!(with_conn.last().unwrap().1, PaletteAction::RestoreDatabase);
        assert_eq!(with_conn[with_conn.len() - 2].1, PaletteAction::BackupDatabase);
    }

    // --- G14 T10: theme toggle is an unconditional fixed action ---
    #[test]
    fn theme_toggle_action_is_always_present() {
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items.iter().any(|i| matches!(
            i,
            PaletteItem::Action { action: PaletteAction::ToggleTheme, .. }
        )));
        // and it fuzzy-matches by its Czech label:
        let items = rank_items("motiv", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items.iter().any(|i| matches!(
            i,
            PaletteItem::Action { action: PaletteAction::ToggleTheme, .. }
        )));
    }

    // --- G14 T11: chart entry gated on chart_available (active Grid tab) ---
    #[test]
    fn chart_entry_present_only_when_a_grid_tab_is_active() {
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, true, false, false);
        assert!(items.iter().any(|i| matches!(
            i,
            PaletteItem::Action { action: PaletteAction::OpenChart, .. }
        )));
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items.iter().all(|i| !matches!(
            i,
            PaletteItem::Action { action: PaletteAction::OpenChart, .. }
        )));
    }

    // --- App-wide master password UX design §2/§3/§7: the two vault
    // actions, each gated independently on its own boolean. ---

    #[test]
    fn unlock_vault_action_present_only_when_unlockable() {
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, true, false);
        assert!(items
            .iter()
            .any(|i| matches!(i, PaletteItem::Action { action: PaletteAction::UnlockVault, .. })));

        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items
            .iter()
            .all(|i| !matches!(i, PaletteItem::Action { action: PaletteAction::UnlockVault, .. })));
    }

    #[test]
    fn lock_vault_action_present_only_when_lockable() {
        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, true);
        assert!(items
            .iter()
            .any(|i| matches!(i, PaletteItem::Action { action: PaletteAction::LockVault, .. })));

        let items = rank_items("", &[], &[], &[], false, AdminEntry::Hidden, 30, false, false, false, false);
        assert!(items
            .iter()
            .all(|i| !matches!(i, PaletteItem::Action { action: PaletteAction::LockVault, .. })));
    }

    #[test]
    fn vault_actions_never_both_absent_or_present_when_only_one_gate_is_set() {
        // Not a structural guarantee (the two params are independent bools,
        // deliberately — see `fixed_actions`'s doc comment on why this
        // isn't one three-state action) — just pins that each row's
        // presence tracks its OWN gate, not the other one's.
        let unlockable_only = fixed_actions(false, AdminEntry::Hidden, false, false, true, false);
        assert!(unlockable_only.iter().any(|(_, a)| matches!(a, PaletteAction::UnlockVault)));
        assert!(!unlockable_only.iter().any(|(_, a)| matches!(a, PaletteAction::LockVault)));

        let lockable_only = fixed_actions(false, AdminEntry::Hidden, false, false, false, true);
        assert!(!lockable_only.iter().any(|(_, a)| matches!(a, PaletteAction::UnlockVault)));
        assert!(lockable_only.iter().any(|(_, a)| matches!(a, PaletteAction::LockVault)));
    }

    // Extends `backup_restore_actions_present_and_last_when_connection_active`:
    // the design requires the vault rows to be inserted BEFORE the
    // backup/restore block (like `OpenChart`), so backup/restore must stay
    // the literal last two rows even when a vault row is also present.
    #[test]
    fn backup_restore_stay_last_two_rows_alongside_a_vault_action() {
        let actions = fixed_actions(false, AdminEntry::Hidden, true, false, true, false);
        assert_eq!(actions.last().unwrap().1, PaletteAction::RestoreDatabase);
        assert_eq!(actions[actions.len() - 2].1, PaletteAction::BackupDatabase);
        assert!(actions.iter().any(|(_, a)| matches!(a, PaletteAction::UnlockVault)));

        let actions = fixed_actions(false, AdminEntry::Hidden, true, false, false, true);
        assert_eq!(actions.last().unwrap().1, PaletteAction::RestoreDatabase);
        assert_eq!(actions[actions.len() - 2].1, PaletteAction::BackupDatabase);
        assert!(actions.iter().any(|(_, a)| matches!(a, PaletteAction::LockVault)));
    }
}
