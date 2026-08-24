# Sidebar Connections Rework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers per task, a **sonnet** adversarial review per task, and a **default-model** final review once all tasks land (G13/G15/G16 staffing convention). NO docker, NO external server anywhere in this phase: every test is a pure `#[test]` over plain data or an embedded sqlite/duckdb `tempfile` path.

**Goal:** Replace the single-connection schema tree with a DataGrip-style multi-root sidebar — every saved connection is an expandable node, each connection expands into all databases on that server, each database into the existing schema tree — and widen the app's active context from "a connection" to "(connection, database)", strengthening every write guard in the process.

**Architecture:** Three layers (design §1–§3): (1) **Identity core** — `conn_identity_for(id, db) = "{id}\u{1F}{db}"` replaces the bare-id identity; the ~26 stamp sites and 9 guard sites inherit the stricter semantics with zero edits because they all funnel through `current_conn_identity()`/`conn_identity_matches()`; the four duplicate spec-resolution sites collapse into one `resolve_active()`. (2) **Lazy multi-root state** — a `SidebarRow` wrapper multi-root over the UNCHANGED `NodeId` inner tree; per-`(conn, db)` slots with generation-guarded lazy fetches (`DbListState`/`DbSchemaState`), LRU-capped snapshot cache, honest indicators (● = active context, children = cached metadata; NO fake "connected" lamp — the runner is per-operation, there is no socket to report). (3) **ON-flip discipline** — the state layer + pure `flatten_sidebar` land fully tested (T4) while the old tree still drives the UI; the user-visible swap is one later task (T5).

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no new primitive; `uniform_list` stays), no new dependencies anywhere.

**Spec:** `docs/superpowers/specs/drafts/sidebar-connections-design.md` — binding, user-approved. Every symbol below is grounded against branch `feature/sidebar-connections` (off main v0.19.0, post-G15+G16). The design doc's line numbers were taken at v0.16.0 — **always re-locate by symbol, never by line number**.

**Resolved deviations from the design doc** (each also called out inline at its task):

1. **Version:** design says "the next free minor after G16's actual number" — G16 shipped **0.19.0**, so T8 bumps to **0.20.0**.
2. **MSSQL cap syntax:** the design's §3.2 table says "LIMIT 2001 appended" — T-SQL has no `LIMIT`. The MSSQL listing uses `SELECT TOP (2001) …`; the pg listing keeps `LIMIT 2001`. Each engine's cap is baked into its pinned SQL const (T2).
3. **File engines go through the runner uniformly** (design §3.2 said main.rs answers synchronously): `fetch_database_list` resolves Sqlite/Duckdb immediately from `cfg.database` WITHOUT opening a connection — the oneshot resolves before the first await, so there is no user-visible latency, and there is exactly ONE dispatch path instead of two that could diverge. No vault prompt fires for them (`engine_is_file_based` gates the prompt away before dispatch).
4. **`fetch_database_list` takes no `engine` parameter** (design §3.2 sketched one): the engine is read from the spec's own `cfg.engine` — a redundant parameter that could disagree with the spec it rides with is a bug shape, not an API.
5. **File-engine `DbNode.name` holds the spec-level string** (full `cfg.database` path), NOT the design's `file_stem(cfg.database)`: a name that cannot round-trip through `spec_for_database` must not live in the data model. The file stem is display-only (`display_db_name`, T4). The Database row itself is KEPT for file engines (design §6's own decision: one uniform hierarchy, and the row doubles as the double-click switch target).
6. **DuckDB exists now** (the design §3.2 table anticipated it): a DuckDB connection has exactly one database (the file) — it takes the same single-`DbNode` shape as sqlite everywhere. No `ATTACH` support (guarded write keyword; multi-file topology out of scope, unchanged from the design).
7. **`spec_for_database` lands in T3 (main.rs), not the runner task** — the design's own §3.1 places it "next to `resolve_active`"; moving it out of the runner task makes T2 file-disjoint and parallelizable with T1.
8. **Store-key call sites land in T3 with the identity core, not the sweep:** the moment `current_conn_identity()` changes format, the params store (which keys by it today) would orphan every saved value for the whole mid-phase window. The legacy-key-for-default rule (design §7 items 4–5) must land in the SAME task as the widening.
9. **`PendingAfterUnlock::{ExpandConnection, SwitchDatabase, LoadDbSchema}` land in T5, not T6:** adding variants makes `resume_pending`'s match non-exhaustive — the compiler forces the resume arms into the same task as the expand/switch handlers they resume. `LoadDbSchema` is a design GAP fix: the vault can be locked (palette "Zamknout trezor") between expanding a connection and later expanding one of its databases; design §4.4 ("no path fetches metadata with an empty secret as a fallback") requires the gate on EVERY metadata fetch, so `LoadSchema` needs its own resumable pending, not just `LoadDatabases`.
10. **`switch_to_connection` becomes a delegating wrapper inside T5** (design put the dropdown rewire in T6): if the old body survived past the introduction of `active_database`, a cross-connection dropdown switch would leave a stale `active_database` behind — a wrong-database dispatch window. The wrapper (`switch_to_database(id, None, cx)`) lands in the same task as the field's first writer.
11. **Same-connection db switch with dirty staged ADMIN edits gets a confirm prompt** (design §5 row 4 said "dropped with the existing warning"; the user's risk list says "release-note + confirm"): `switch_to_database` routes through the existing `discard_confirm` infrastructure with a new `PendingDiscard::SwitchDatabase` variant when an open admin tab stamped with the CURRENT identity has `change_count() > 0`. Dirty sandbox GRID edits deliberately get NO prompt: they are not dropped — the tab stays, the apply bar dims, and switching back to the same `(conn, db)` re-enables it (pinned by test, T7).
12. **CLI-arg root has NO Database level** (design §3.4's "expandable straight into the single fetched snapshot" read literally): the CLI session cannot switch databases, so a Database row would be a dead switch target — schema rows splice directly under the synthetic root with `db = ""`. This is the opposite call from file engines (deviation 5), where the row IS the switch target.
13. **`DbSchemaState::Loading` carries `prev_expanded`** (not in the design's sketch): a ⟳ refresh of a Loaded slot must carry its expand-set forward through the Loading transition into `prune_stale_ids`, or the same-slot-refresh-preserves-expansion contract (design §1.2) is silently lost.
14. **`SidebarRow::Pinned(NodeId)` variant added** (the design §1.1 enum sketch had no variant for the pinned "Správa serveru"/"Oblíbené" rows even though its own tree diagram renders them): pinned rows reuse the existing `NodeId::{AdminRoot, FavouriteSection, Favourite}` values wrapped in one variant, so their click/double-click semantics stay the pre-rework code paths verbatim.

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test (except the final-gate builds in T8).
- **Zero warnings** in plain AND test builds, debug AND release profiles, for every crate touched. New pub items get doc comments; no `#[allow(dead_code)]` without a named removal owner.
- GPUI pin `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no upgrade, no new primitives; `uniform_list` for all sidebar rows.
- **No `_ =>` wildcard over `Engine` anywhere** (house rule) — the DB-list dispatch match in T2 is exhaustive over `Postgres | Mssql | Sqlite | Duckdb`.
- **No `USE`/`\c` is ever sent** (design §3.3, binding): every per-database operation builds a fresh `ConnectSpec` via `spec_for_database`. Session-level switching would desynchronize the identity from the wire state.
- **`\u{1F}` identity convention:** ids are `conn-{nanos:x}` (`generate_connection_id`), pg/mssql database names and Windows file paths cannot contain control characters, so `"{id}\u{1F}{db}"` cannot collide with either component. The composite identity is **never rendered raw** — only `conn_name_for_identity` produces display text.
- **Secrets:** `spec_for_database` moves a field it never reads — no new storage, logging, or formatting of any secret. The vault prompt fronts every first-touch of a server needing a secret (`connect_needs_vault_prompt` gate at every metadata-fetch/switch entry); no path fetches metadata with an empty secret as a fallback.
- **`read_only` is a property of the SAVED connection** and inherits into every derived database spec (server-side enforcement — pg `default_transaction_read_only=on`, SQLite/DuckDB read-only open modes, MSSQL's client-side-only guard — applies uniformly). No per-database override.
- **Write paths unchanged:** sandbox Apply, admin apply, script runner, CSV import, monitor kill — all behind identity guards this phase STRENGTHENS. No new sanctioned `Connection::execute` caller.
- **Caps:** `LOADED_SNAPSHOT_CAP = 8` (LRU, never evicts the active slot), `DB_LIST_CAP = 2000` (+1 sentinel row for truncation detection, disclosed in-UI).
- **Speed search filters LOADED content only** (binding): `flatten_sidebar` is pure — typing can never trigger a fetch (holds by construction; doc-commented, not tested).
- Czech user-facing strings exactly as quoted in the tasks below.
- **Single-writer serialized files:** `main.rs`, `schema_tree.rs`, `connections_ui.rs`, `runner.rs` are single-writer per batch. T5 runs SOLO (touches three of them).
- **Merge gate** (every task from T3 on, and finally T8): `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` green, zero warnings. dbc-mcp builds its own specs from config and is default-db by construction — expected diff there: NONE; the gate proves it stays green.
- **Versioning:** T8 bumps `[workspace.package] version` (root `Cargo.toml`, currently `0.19.0`) to `0.20.0` (re-check main at merge time per convention).

### Task dependency graph

| Task | Name | Depends on | Files | Batch |
|---|---|---|---|---|
| T1 | dbc-state: `FavouriteObject.database` + `connection_scope_key` | — | `dbc-state/src/{config.rs,scope.rs,lib.rs,view_prefs.rs,params.rs}` (last two: tests only) | A (parallel) |
| T2 | Runner: `fetch_database_list` + pinned per-engine SQL | — | `dbc-ui/src/runner.rs` | A (parallel) |
| T3 | Identity core: `active_database`, `conn_identity_for`, `resolve_active`, `spec_for_database`, store keys | T1 | `dbc-ui/src/main.rs` | B (parallel) |
| T4 | schema_tree.rs ADDITIVE multi-root state layer + `flatten_sidebar` | T1 | `dbc-ui/src/schema_tree.rs`, `dbc-ui/src/connections_ui.rs` (two one-line `pub(crate)` visibility edits only) | B (parallel) |
| T5 | THE FLIP: entity + render + TreeEvent scope + main.rs wiring + `switch_to_database` | T2, T3, T4 | `dbc-ui/src/{schema_tree.rs,main.rs,connections_ui.rs}` | C (SOLO) |
| T6 | connections_ui.rs: top-bar label, dropdown demotion, compare db sub-pick | T5 | `dbc-ui/src/connections_ui.rs`, `dbc-ui/src/compare.rs` (if labels live there) | D |
| T7 | Identity-widening AUDIT: full stamp/guard site walk + no-guard-got-weaker tests | T5, T6 | `dbc-ui/src/main.rs` (tests + comments only) | E |
| T8 | Sweep: history label, docs/release notes, version 0.20.0, full gates | all | `dbc-ui/src/history_panel.rs`, root `Cargo.toml`, docs | last |

Suggested batches: **{T1, T2}** parallel → **{T3, T4}** parallel → **{T5}** solo → **{T6}** → **{T7}** → **{T8}**.

---

### Task 1 (T1): dbc-state — `FavouriteObject.database` + `connection_scope_key`

**Files:**
- Modify: `crates/dbc-state/src/config.rs` (FavouriteObject + tests)
- Create: `crates/dbc-state/src/scope.rs`
- Modify: `crates/dbc-state/src/lib.rs` (one `mod` + one `pub use` line)
- Modify: `crates/dbc-state/src/view_prefs.rs`, `crates/dbc-state/src/params.rs` (tests only — the stores themselves DON'T change; the key discipline lives in what callers pass as `connection_id`)

**Interfaces (produced; consumed by T3, T4, T5):**

```rust
// config.rs — additive field, old files load AND round-trip byte-identically
pub struct FavouriteObject {
    pub connection_id: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: String,
    /// Sidebar rework (design §5 row 9): the database this favourite lives
    /// in. `None` = the connection's DEFAULT database (whatever
    /// `ConnectionConfig::database` says at read time), so every existing
    /// config.toml entry keeps meaning exactly what it meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

// scope.rs
pub fn connection_scope_key(connection_id: &str, database: Option<&str>) -> String;
```

**Why the stores don't change:** `ViewPrefsStore`/`ParamValuesStore` already take an opaque `connection_id: &str` and `\u{1F}`-join it with the rest of the key. The legacy-for-default rule (design §7 items 4–5) is entirely about WHAT the caller passes: bare id for the default database (existing files keep working byte-for-byte), `"{id}\u{1F}{db}"` for a non-default one. `connection_scope_key` is that rule, in one place, in the crate both stores live in.

- [ ] **Step 1: Write the failing tests.** In `config.rs`'s existing `mod tests`, next to the existing back-compat tests:

```rust
    #[test]
    fn favourite_without_database_field_loads_and_roundtrips_byte_identically() {
        // Sidebar rework: `database` is additive with serde(default) +
        // skip_serializing_if — an old config.toml must load AND save back
        // without gaining the field (same posture as G16's variant pin).
        let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"

[[favourite_objects]]
connection_id = "c1"
name = "orders"
kind = "table"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.favourite_objects[0].database, None);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        config.save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("database = ") || raw.matches("database = ").count() == 1,
            "favourite must not serialize a database key when None (only the connection's own): {raw}");
        let reloaded = AppConfig::load(&p).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn favourite_with_database_roundtrips() {
        let mut config = AppConfig::default();
        config.favourite_objects.push(FavouriteObject {
            connection_id: "c1".into(),
            schema: Some("public".into()),
            name: "orders".into(),
            kind: "table".into(),
            database: Some("inventory".into()),
        });
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        config.save(&p).unwrap();
        let reloaded = AppConfig::load(&p).unwrap();
        assert_eq!(reloaded.favourite_objects[0].database.as_deref(), Some("inventory"));
    }

    #[test]
    fn toggle_favourite_distinguishes_databases() {
        // Full-struct equality in toggle_favourite means the same table in
        // two databases is two distinct favourites — pin it.
        let mut config = AppConfig::default();
        let f_default = FavouriteObject {
            connection_id: "c1".into(), schema: None, name: "t".into(),
            kind: "table".into(), database: None,
        };
        let f_other = FavouriteObject { database: Some("inventory".into()), ..f_default.clone() };
        assert!(config.toggle_favourite(f_default.clone()));
        assert!(config.toggle_favourite(f_other.clone()));
        assert_eq!(config.favourite_objects.len(), 2);
        assert!(!config.toggle_favourite(f_default)); // removes only the default-db one
        assert_eq!(config.favourite_objects.len(), 1);
    }
```

  New file `crates/dbc-state/src/scope.rs`:

```rust
//! Sidebar rework (design §5 row 10, §7 items 4–5): the store-bucket key
//! rule shared by view_prefs and params callers.

/// The `connection_id` value callers should hand to `ViewPrefsStore`/
/// `ParamValuesStore` once the app's active context is `(connection,
/// database)`:
///
/// - `database == None` (the connection's DEFAULT database): the LEGACY
///   bare id, byte-identical to every key written before this phase —
///   existing views.toml/params.toml entries keep working with no rewrite
///   and no loss.
/// - `database == Some(db)` (a non-default database): one more
///   `\u{1F}`-separated component. The stores' own `encode_key` appends
///   further components with the same separator; keys stay unambiguous
///   because the id itself (`conn-{hex}`) and database names/file paths
///   can never contain the control character.
///
/// The COMPOSITE conn identity (`"{id}\u{1F}{db}"` for the default db too)
/// is deliberately NOT used here: it would orphan every existing stored
/// value (design §7 item 5's collision check).
pub fn connection_scope_key(connection_id: &str, database: Option<&str>) -> String {
    match database {
        None => connection_id.to_string(),
        Some(db) => format!("{connection_id}\u{1F}{db}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_db_key_is_the_legacy_bare_id() {
        assert_eq!(connection_scope_key("conn-abc", None), "conn-abc");
    }

    #[test]
    fn non_default_db_appends_one_separated_component() {
        assert_eq!(connection_scope_key("conn-abc", Some("sales")), "conn-abc\u{1F}sales");
    }

    #[test]
    fn different_databases_isolate() {
        assert_ne!(
            connection_scope_key("conn-abc", Some("sales")),
            connection_scope_key("conn-abc", Some("inventory"))
        );
        assert_ne!(connection_scope_key("conn-abc", Some("sales")), connection_scope_key("conn-abc", None));
    }
}
```

  In `lib.rs` add:

```rust
mod scope;
pub use scope::connection_scope_key;
```

  In `view_prefs.rs`'s `mod tests` and `params.rs`'s `mod tests`, one store-level isolation test each (the "two tests each" of design §5 row 10 — the legacy-round-trip half is scope.rs's `default_db_key_is_the_legacy_bare_id` plus these):

```rust
    // view_prefs.rs tests
    #[test]
    fn scope_key_isolates_databases_and_preserves_legacy_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("views.toml");
        let mut store = ViewPrefsStore::load(&p).unwrap();
        let legacy = TableViewPrefs { hidden_columns: vec!["a".into()], ..Default::default() };
        let other = TableViewPrefs { hidden_columns: vec!["b".into()], ..Default::default() };
        // Written pre-phase with the bare id:
        store.set("conn-1", Some("public"), "t", legacy.clone()).unwrap();
        // Written post-phase for a non-default db:
        let scoped = crate::connection_scope_key("conn-1", Some("inventory"));
        store.set(&scoped, Some("public"), "t", other.clone()).unwrap();
        let loaded = ViewPrefsStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn-1", Some("public"), "t"), Some(&legacy));
        assert_eq!(loaded.get(&scoped, Some("public"), "t"), Some(&other));
    }
```

```rust
    // params.rs tests
    #[test]
    fn scope_key_isolates_databases_and_preserves_legacy_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");
        let mut store = ParamValuesStore::load(&p).unwrap();
        let legacy = ParamValue { text: "1".into(), is_null: false };
        let scoped_v = ParamValue { text: "2".into(), is_null: false };
        store.set("conn-1", "id", legacy.clone()).unwrap();
        let scoped = crate::connection_scope_key("conn-1", Some("inventory"));
        store.set(&scoped, "id", scoped_v.clone()).unwrap();
        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn-1", "id"), Some(&legacy));
        assert_eq!(loaded.get(&scoped, "id"), Some(&scoped_v));
    }
```

- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state` (compile errors: no `database` field, no `scope` module).
- [ ] **Step 3: Implement** — add the field to `FavouriteObject` exactly as in Interfaces (every existing `FavouriteObject { .. }` literal in dbc-state AND dbc-ui gains `database: None` — the compiler enumerates them; fix the dbc-state ones now, note that dbc-ui breaks until its own literals are fixed: grep `FavouriteObject {` in `crates/dbc-ui/src` and add `database: None` to each — this is a cross-crate compile fix, allowed here because it is mechanical and T4 depends on it building).
- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state -p dbc-ui` (dbc-ui must still compile+pass with `database: None` literals; zero warnings).
- [ ] **Step 5: Commit** — `feat(state): FavouriteObject.database + connection_scope_key legacy-for-default rule (sidebar T1)`.

---

### Task 2 (T2): runner.rs — `fetch_database_list` + pinned per-engine SQL

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`

**Interfaces (produced; consumed by T5):**

```rust
pub const DB_LIST_CAP: usize = 2000;
pub const PG_DB_LIST_SQL: &str;      // LIMIT 2001 baked in
pub const MSSQL_DB_LIST_SQL: &str;   // TOP (2001) baked in
pub fn truncate_db_list(names: Vec<String>) -> (Vec<String>, bool);
impl QueryRunner {
    /// Ok((database names, truncated)) — truncated = server had > DB_LIST_CAP.
    pub fn fetch_database_list(
        &self,
        spec: ConnectSpec,
    ) -> tokio::sync::oneshot::Receiver<Result<(Vec<String>, bool), QueryError>>;
}
```

- [ ] **Step 1: Write the failing tests** in `runner.rs`'s tests (new `mod db_list_tests` next to the other pure-fn test mods):

```rust
#[cfg(test)]
mod db_list_tests {
    use super::*;

    /// The two catalog queries are saved-behaviour contracts (design §3.2):
    /// pg excludes templates AND `datallowconn = false` (deliberately
    /// stricter than admin_sql's sizes query — a db you cannot connect to
    /// must not render as expandable); MSSQL takes ONLINE only (state = 0)
    /// and deliberately INCLUDES system DBs (DataGrip precedent). Each cap
    /// is dialect-native: `LIMIT` is not T-SQL, `TOP` is not Postgres —
    /// resolved deviation 2.
    #[test]
    fn db_list_sql_texts_are_pinned() {
        assert_eq!(
            PG_DB_LIST_SQL,
            "SELECT datname FROM pg_catalog.pg_database \
             WHERE NOT datistemplate AND datallowconn ORDER BY datname LIMIT 2001"
        );
        assert_eq!(
            MSSQL_DB_LIST_SQL,
            "SELECT TOP (2001) name FROM sys.databases WHERE state = 0 ORDER BY name"
        );
    }

    #[test]
    fn truncate_db_list_caps_at_2000_and_flags() {
        let names: Vec<String> = (0..2001).map(|i| format!("db{i:04}")).collect();
        let (kept, truncated) = truncate_db_list(names);
        assert_eq!(kept.len(), DB_LIST_CAP);
        assert!(truncated);
        let (kept, truncated) = truncate_db_list(vec!["a".into(), "b".into()]);
        assert_eq!(kept.len(), 2);
        assert!(!truncated);
        // Exactly at the cap: NOT truncated (the +1 sentinel row is the signal).
        let names: Vec<String> = (0..2000).map(|i| format!("db{i:04}")).collect();
        assert!(!truncate_db_list(names).1);
    }

    fn file_cfg(engine: dbc_state::Engine, path: &str) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "f1".into(), name: "file".into(), folder: vec![],
            engine, host: String::new(), port: None,
            database: path.into(), user: String::new(), read_only: false,
            timeout_secs: None, auto_limit: None, ssh: None,
            favourite: false, mssql: None,
        }
    }

    /// Resolved deviation 3/5: file engines answer from `cfg.database`
    /// (the SPEC-LEVEL string — full path, not the display stem) without
    /// opening a connection; the oneshot resolves immediately.
    #[test]
    fn file_engines_resolve_immediately_from_the_config_path() {
        let runner = QueryRunner::new();
        for engine in [dbc_state::Engine::Sqlite, dbc_state::Engine::Duckdb] {
            let spec = ConnectSpec::Config {
                cfg: Box::new(file_cfg(engine, r"D:\data\analytics.duckdb")),
                secret: None,
            };
            let (dbs, truncated) = runner
                .fetch_database_list(spec)
                .blocking_recv()
                .expect("sender must not drop")
                .expect("file engines never error here");
            assert_eq!(dbs, vec![r"D:\data\analytics.duckdb".to_string()]);
            assert!(!truncated);
        }
    }

    /// CLI/URL specs never reach the tree's listing path (the CLI root has
    /// no db list, design §3.4) — the defensive arm refuses honestly
    /// rather than guessing an engine.
    #[test]
    fn url_spec_is_refused() {
        let runner = QueryRunner::new();
        let err = runner
            .fetch_database_list(ConnectSpec::Url("postgres://x/y".into()))
            .blocking_recv()
            .unwrap()
            .unwrap_err();
        assert_eq!(err.message, "výpis databází není pro CLI připojení k dispozici");
    }
}
```

- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui db_list` (compile error: names not defined).
- [ ] **Step 3: Implement.** Next to `fetch_schema`:

```rust
/// Design §6: the DB-list cap. The listing SQL carries a `2001`-row cap in
/// its own dialect (`LIMIT` / `TOP`) and `drain_all_rows` is capped at
/// `DB_LIST_CAP + 1` as a second belt — the sentinel 2001st row is how
/// `truncate_db_list` detects "there were more" without a COUNT round-trip.
pub const DB_LIST_CAP: usize = 2000;

/// Design §3.2: excludes templates AND `datallowconn = false` —
/// deliberately stricter than `admin_sql`'s sizes query (templates only):
/// a database you cannot connect to must not render as an expandable row.
pub const PG_DB_LIST_SQL: &str = "SELECT datname FROM pg_catalog.pg_database \
     WHERE NOT datistemplate AND datallowconn ORDER BY datname LIMIT 2001";

/// Design §3.2: ONLINE databases only (state = 0). System DBs
/// (master/msdb/model/tempdb) are INCLUDED — DataGrip precedent; hiding
/// them would surprise admins, and they are just rows until expanded.
/// `TOP (2001)`, not `LIMIT` — resolved deviation 2.
pub const MSSQL_DB_LIST_SQL: &str =
    "SELECT TOP (2001) name FROM sys.databases WHERE state = 0 ORDER BY name";

/// Pure half of the truncation contract: keep at most `DB_LIST_CAP` names,
/// flag whether anything was dropped (the caller renders the disclosure
/// Notice row, design §6).
pub fn truncate_db_list(mut names: Vec<String>) -> (Vec<String>, bool) {
    if names.len() > DB_LIST_CAP {
        names.truncate(DB_LIST_CAP);
        (names, true)
    } else {
        (names, false)
    }
}
```

  and the method on `QueryRunner` (same open/run/drop shape as `fetch_schema`; reuses `drain_all_rows`):

```rust
    /// Sidebar rework (design §3.2): one-shot list of the server's
    /// databases, over a short-lived connection to the spec's own database
    /// (the caller passes the DEFAULT-database spec), under the
    /// connection's own privileges and read-only session — zero privilege
    /// escalation; a denied catalog read degrades to the caller's error
    /// row, never a retry loop or a privilege prompt (design §4.5).
    ///
    /// File engines (Sqlite/Duckdb) resolve IMMEDIATELY from
    /// `cfg.database` without opening a connection — one file, one
    /// database (resolved deviations 3–5). `Url` specs are refused: the
    /// CLI root never lists (design §3.4).
    pub fn fetch_database_list(
        &self,
        spec: ConnectSpec,
    ) -> tokio::sync::oneshot::Receiver<Result<(Vec<String>, bool), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let engine = match &spec {
            ConnectSpec::Config { cfg, .. } => cfg.engine,
            ConnectSpec::Url(_) => {
                let _ = tx.send(Err(QueryError::msg(
                    "výpis databází není pro CLI připojení k dispozici",
                )));
                return rx;
            }
        };
        // Exhaustive over Engine — house rule, no `_ =>`.
        let sql = match engine {
            dbc_state::Engine::Sqlite | dbc_state::Engine::Duckdb => {
                let ConnectSpec::Config { cfg, .. } = &spec else { unreachable!("matched above") };
                let _ = tx.send(Ok((vec![cfg.database.clone()], false)));
                return rx;
            }
            dbc_state::Engine::Postgres => PG_DB_LIST_SQL,
            dbc_state::Engine::Mssql => MSSQL_DB_LIST_SQL,
        };
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = async {
                let mut opened = open_spec(spec, handle).await?;
                let (_cols, rows) =
                    drain_all_rows(opened.conn.as_mut(), sql, DB_LIST_CAP + 1).await?;
                let names: Vec<String> =
                    rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
                Ok(truncate_db_list(names))
            }
            .await;
            let _ = tx.send(result);
        });
        rx
    }
```

  (If `drain_all_rows`'s call shape differs — it is `async fn drain_all_rows(conn: &mut dyn Connection, sql: &str, cap: usize) -> Result<AdminCatalogRows, QueryError>` with `AdminCatalogRows = (Vec<String>, Vec<Vec<Option<String>>>)` — match `fetch_admin_catalog_inner`'s existing call exactly.)

- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (all tests, zero warnings).
- [ ] **Step 5: Commit** — `feat(runner): fetch_database_list with pinned pg/mssql catalog SQL and 2000-cap (sidebar T2)`.

---

### Task 3 (T3): main.rs — identity core, `resolve_active`, `spec_for_database`, store keys

**Files:**
- Modify: `crates/dbc-ui/src/main.rs`

**Interfaces (produced; consumed by T5, T6, T7):**

```rust
// free functions (crate root — visible to connections_ui/schema_tree via crate::)
pub(crate) fn conn_identity_for(conn_id: &str, database: &str) -> String; // "{id}\u{1F}{db}"
pub(crate) fn spec_for_database(cfg: &ConnectionConfig, db: &str, secret: Option<String>) -> ConnectSpec;

struct ActiveConn {
    cfg: ConnectionConfig,      // database ALREADY swapped to the effective one
    secret: Option<String>,
    read_only: bool,
    engine: dbc_state::Engine,
    timeout_secs: Option<u64>,
    auto_limit: Option<u64>,
    identity: String,           // conn_identity_for(..) of the same snapshot
}
impl ActiveConn { fn into_spec(self) -> ConnectSpec; }

impl AppView {
    // new field: active_database: Option<String>  (None = saved default; always None when active_connection_id is None)
    fn effective_database(&self) -> Option<String>;
    fn resolve_active(&self) -> Option<ActiveConn>;   // None = no active saved conn OR conn deleted; CLI handled by callers as today
    fn store_scope_key(&self) -> String;              // legacy-for-default bucket key; "cli" sentinel
}
```

**The invariant this task establishes (doc-comment it on `resolve_active`):** *no other code path may build a `ConnectSpec::Config` from `active_connection_id` directly* — `resolve_active` is the single site where "the database the app talks to" is decided. Compare and backup build specs from EXPLICIT configs by design and are exempt (design §2.4, §5 rows 6–7).

- [ ] **Step 1: Write the failing tests.** In `main.rs`, extend `mod conn_identity_matches_tests` and add a new `mod identity_widening_tests` next to it:

```rust
#[cfg(test)]
mod identity_widening_tests {
    use super::*;

    #[test]
    fn conn_identity_for_composes_with_unit_separator() {
        assert_eq!(conn_identity_for("conn-a", "sales"), "conn-a\u{1F}sales");
    }

    /// THE safety win of the whole phase (design §2.3): the same connection
    /// on two databases is two different identities — every pending write
    /// guard (Apply, admin, script, CSV) captured against one refuses to
    /// dispatch against the other, via the unchanged `conn_identity_matches`.
    #[test]
    fn same_connection_different_database_never_matches() {
        assert!(!conn_identity_matches(
            &conn_identity_for("conn-a", "sales"),
            &conn_identity_for("conn-a", "inventory"),
        ));
        assert!(conn_identity_matches(
            &conn_identity_for("conn-a", "sales"),
            &conn_identity_for("conn-a", "sales"),
        ));
        // Bare pre-phase shape never equals the composite (defensive).
        assert!(!conn_identity_matches("conn-a", &conn_identity_for("conn-a", "sales")));
    }

    #[test]
    fn conn_spec_key_distinguishes_databases() {
        let mut cfg = test_cfg("conn-a", "sales");
        let key_sales = conn_spec_key(&ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: None });
        cfg.database = "inventory".into();
        let key_inv = conn_spec_key(&ConnectSpec::Config { cfg: Box::new(cfg), secret: None });
        assert_ne!(key_sales, key_inv);
        assert_eq!(key_sales, "cfg:conn-a\u{1F}sales");
    }

    fn test_cfg(id: &str, db: &str) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: id.into(), name: "prod".into(), folder: vec![],
            engine: dbc_state::Engine::Postgres, host: "localhost".into(),
            port: Some(5432), database: db.into(), user: "u".into(),
            read_only: true, timeout_secs: Some(30), auto_limit: Some(500),
            ssh: None, favourite: false, mssql: None,
        }
    }

    /// SECURITY (design §3.1): the derived spec inherits EVERYTHING except
    /// `database` — same id (⇒ same vault secret, same prefs bucket root),
    /// same read_only (⇒ server-side enforcement still applies), same
    /// timeout/auto_limit/ssh. No new secret storage.
    #[test]
    fn spec_for_database_swaps_only_the_database() {
        let cfg = test_cfg("conn-a", "sales");
        let spec = spec_for_database(&cfg, "inventory", Some("s3cret".into()));
        let ConnectSpec::Config { cfg: derived, secret } = spec else { panic!("Config expected") };
        assert_eq!(derived.database, "inventory");
        assert_eq!(secret.as_deref(), Some("s3cret"));
        let mut expect = cfg.clone();
        expect.database = "inventory".into();
        assert_eq!(*derived, expect); // read_only/timeout/auto_limit/ssh/engine/id all inherited
    }

    #[test]
    fn resolve_active_from_swaps_db_and_inherits_flags() {
        let mut config = dbc_state::AppConfig::default();
        config.connections.push(test_cfg("conn-a", "sales"));
        // Default database:
        let a = resolve_active_from(&config, None, "conn-a", None).unwrap();
        assert_eq!(a.cfg.database, "sales");
        assert_eq!(a.identity, conn_identity_for("conn-a", "sales"));
        assert!(a.read_only);
        assert_eq!(a.timeout_secs, Some(30));
        assert_eq!(a.auto_limit, Some(500));
        // Non-default database:
        let a = resolve_active_from(&config, None, "conn-a", Some("inventory")).unwrap();
        assert_eq!(a.cfg.database, "inventory");
        assert_eq!(a.identity, conn_identity_for("conn-a", "inventory"));
        assert!(a.read_only, "read_only inherits into every derived db (design §4.2)");
        // Deleted connection:
        assert!(resolve_active_from(&config, None, "gone", None).is_none());
    }
}
```

- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui identity_widening` (compile errors: `conn_identity_for`/`spec_for_database`/`resolve_active_from` undefined).
- [ ] **Step 3: Implement the identity core.** All in `main.rs`:

  **(a)** New `AppView` field, directly under `active_connection_id`:

```rust
    /// Sidebar rework (design §2.2): the active database WITHIN
    /// `active_connection_id`. `None` = the saved config's `database` (the
    /// default). Always `None` when `active_connection_id` is `None` (the
    /// CLI path has no db switching) and always NORMALIZED — explicitly
    /// picking the default db stores `None`, so identity/store-key/label
    /// logic has a single canonical spelling (`switch_to_database` enforces
    /// this; until that lands in T5, nothing writes `Some` here).
    active_database: Option<String>,
```

  Initialize `active_database: None,` at the `AppView` construction site in `main()` (the struct literal that also sets `active_connection_id: None`).

  **(b)** Free functions, next to `CLI_CONN_IDENTITY`:

```rust
/// Design §2.3: the widened connection identity. `\u{1F}` (unit separator)
/// joins id and database — the same convention dbc-state's
/// view_prefs/params `encode_key` already uses; it cannot collide with
/// either component (ids are `conn-{hex}`, database names/file paths never
/// contain control characters) and it is NEVER rendered raw
/// (`conn_name_for_identity` translates). The CLI path keeps the plain
/// `"cli"` sentinel (its URL bakes its own database).
pub(crate) fn conn_identity_for(conn_id: &str, database: &str) -> String {
    format!("{conn_id}\u{1F}{database}")
}

/// SECURITY (design §3.1): the derived spec inherits EVERYTHING from the
/// saved config except `database` — same id (⇒ same vault secret, same
/// favourites/prefs bucket root), same read_only (⇒ `open_config` still
/// applies default_transaction_read_only / file-engine read-only modes),
/// same ssh/timeout/auto_limit. No new secret storage, no new config
/// entry; this function moves a secret field it never reads.
pub(crate) fn spec_for_database(
    cfg: &ConnectionConfig,
    db: &str,
    secret: Option<String>,
) -> ConnectSpec {
    let mut cfg = cfg.clone();
    cfg.database = db.to_string();
    ConnectSpec::Config { cfg: Box::new(cfg), secret }
}
```

  **(c)** Widen `conn_spec_key` (its doc comment gains one sentence: *"widened by the sidebar rework — its autocomplete/schema caching identity must distinguish databases; the spec's `cfg.database` is already the effective one"*):

```rust
fn conn_spec_key(spec: &ConnectSpec) -> String {
    match spec {
        ConnectSpec::Config { cfg, .. } => format!("cfg:{}\u{1F}{}", cfg.id, cfg.database),
        ConnectSpec::Url(u) => format!("url:{u}"),
    }
}
```

  **(d)** `effective_database` + recomposed `current_conn_identity` (replace the one-line body; keep the existing doc comment, appending: *"Sidebar rework: composes via `conn_identity_for` — a database switch on the SAME connection now changes the identity, which is the audit's headline fix (design §7)"*):

```rust
    /// The database the active context points at: `active_database`, or
    /// the saved config's default. `None` = no active saved connection.
    fn effective_database(&self) -> Option<String> {
        let id = self.active_connection_id.as_ref()?;
        if let Some(db) = &self.active_database {
            return Some(db.clone());
        }
        self.config.connections.iter().find(|c| &c.id == id).map(|c| c.database.clone())
    }

    fn current_conn_identity(&self) -> String {
        match &self.active_connection_id {
            None => CLI_CONN_IDENTITY.to_string(),
            Some(id) => {
                // A deleted-while-active connection (rare, transient) falls
                // back to the empty db component — still a stable, unequal-
                // to-everything-real identity, same posture as the old raw
                // id fallback.
                let db = self.effective_database().unwrap_or_default();
                conn_identity_for(id, &db)
            }
        }
    }
```

  **(e)** `conn_name_for_identity` learns to split (doc comment gains: *"splits on `\u{1F}`; the db segment renders only when ≠ the connection's current default"*):

```rust
    fn conn_name_for_identity(&self, identity: &str) -> String {
        if identity == CLI_CONN_IDENTITY {
            return "cli".to_string();
        }
        let (id, db) = match identity.split_once('\u{1F}') {
            Some((id, db)) => (id, Some(db)),
            None => (identity, None), // defensive: nothing stamps the bare shape any more
        };
        match self.config.connections.iter().find(|c| c.id == id) {
            // Deleted connection: never render the raw control character.
            None => identity.replace('\u{1F}', " / "),
            Some(c) => match db {
                Some(db) if db != c.database => format!("{} / {}", c.name, db),
                _ => c.name.clone(),
            },
        }
    }
```

  **(f)** `ActiveConn` + `resolve_active_from` + `resolve_active` (place next to `active_conn_spec`):

```rust
/// Design §2.4: the ONE resolved snapshot of the active `(connection,
/// database)` context. INVARIANT (design §2.4, doc'd here as the single
/// change point): no other code path may build a `ConnectSpec::Config`
/// from `active_connection_id` directly — `run_query_with`,
/// `resolve_spec_for_explain`, `apply_conn_spec` and `active_conn_spec`
/// are all thin projections of this. Compare and backup build specs from
/// EXPLICIT configs by design and are exempt (design §5 rows 6–7).
struct ActiveConn {
    /// `database` ALREADY swapped to the effective one.
    cfg: ConnectionConfig,
    secret: Option<String>,
    read_only: bool,
    engine: dbc_state::Engine,
    timeout_secs: Option<u64>,
    auto_limit: Option<u64>,
    /// `conn_identity_for(..)` of the same snapshot — callers that stamp
    /// and dispatch in one motion use this, never a re-read.
    identity: String,
}

impl ActiveConn {
    fn into_spec(self) -> ConnectSpec {
        ConnectSpec::Config { cfg: Box::new(self.cfg), secret: self.secret }
    }
}

/// Pure core of `AppView::resolve_active` — free function so it is
/// testable without a GPUI context (this crate has no GPUI test harness).
fn resolve_active_from(
    config: &AppConfig,
    vault: Option<&Vault>,
    active_id: &str,
    active_db: Option<&str>,
) -> Option<ActiveConn> {
    let saved = config.connections.iter().find(|c| c.id == active_id)?;
    let mut cfg = saved.clone();
    if let Some(db) = active_db {
        cfg.database = db.to_string();
    }
    let secret = connect::resolve_secret_for_connect(vault, &cfg);
    Some(ActiveConn {
        identity: conn_identity_for(&cfg.id, &cfg.database),
        read_only: cfg.read_only,
        engine: cfg.engine,
        timeout_secs: cfg.timeout_secs,
        auto_limit: cfg.auto_limit,
        secret,
        cfg,
    })
}
```

  and on `AppView`:

```rust
    fn resolve_active(&self) -> Option<ActiveConn> {
        let id = self.active_connection_id.as_deref()?;
        resolve_active_from(&self.config, self.vault.as_ref(), id, self.active_database.as_deref())
    }
```

- [ ] **Step 4: Collapse the four duplicate resolution sites onto `resolve_active`** (design §0.4/§2.4). Each keeps its CLI/`conn_url` else-arm byte-for-byte:

  **(a)** `run_query_with`'s inline block — replace the `if let Some(id) = self.active_connection_id.clone()` arm with:

```rust
        let spec = if self.active_connection_id.is_some() {
            let Some(a) = self.resolve_active() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return;
            };
            let conn_meta = Some((a.read_only, a.engine));
            (a.read_only, a.auto_limit, a.timeout_secs, conn_meta, a.into_spec())
        } else if let Some(url) = self.conn_url.clone() {
            // ... existing CLI arm, unchanged ...
```

  **(b)** `resolve_spec_for_explain` — replace its first arm with:

```rust
        if self.active_connection_id.is_some() {
            let Some(a) = self.resolve_active() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return None;
            };
            Some((a.read_only, a.timeout_secs, a.engine, a.into_spec()))
        } else if let Some(url) = self.conn_url.clone() {
            // ... existing CLI arm, unchanged ...
```

  **(c)** `active_conn_spec`:

```rust
    fn active_conn_spec(&self) -> Option<ConnectSpec> {
        if self.active_connection_id.is_some() {
            self.resolve_active().map(ActiveConn::into_spec)
        } else {
            self.conn_url.clone().map(ConnectSpec::Url)
        }
    }
```

  **(d)** `apply_conn_spec`:

```rust
    fn apply_conn_spec(&self) -> Option<(ConnectSpec, Option<u64>)> {
        if self.active_connection_id.is_some() {
            self.resolve_active().map(|a| {
                let timeout_secs = a.timeout_secs;
                (a.into_spec(), timeout_secs)
            })
        } else {
            self.conn_url.clone().map(|url| (ConnectSpec::Url(url), None))
        }
    }
```

  **(e)** Sweep check: `grep -n "resolve_secret_for_connect" crates/dbc-ui/src/main.rs` — every remaining hit must be one of: inside `resolve_active_from`, the compare/backup explicit-config paths, or `connections_ui.rs`'s switch path (rewired in T5). Any OTHER hit that pairs it with `active_connection_id` is a missed duplicate — route it through `resolve_active` and record it in the commit message.

- [ ] **Step 5: Store keys (legacy-for-default).** Add:

```rust
    /// Store bucket key for view_prefs/params (design §7 items 4–5):
    /// LEGACY bare id for the default database — existing views.toml/
    /// params.toml entries keep working byte-for-byte — one more `\u{1F}`
    /// component only for a non-default db; `"cli"` sentinel for the CLI
    /// path. Deliberately NOT `current_conn_identity()`: embedding the
    /// composite identity would orphan every pre-phase stored value.
    fn store_scope_key(&self) -> String {
        match &self.active_connection_id {
            Some(id) => dbc_state::connection_scope_key(id, self.active_database.as_deref()),
            None => CLI_CONN_IDENTITY.to_string(),
        }
    }
```

  Rewire the four call sites:
  - `open_query_params_dialog`: `let conn_id = self.current_conn_identity();` → `let conn_id = self.store_scope_key();`
  - `confirm_query_params`: same one-line change.
  - `apply_view_prefs_to_grid`: `let conn_id = self.active_connection_id.clone()?;` → `let conn_id = self.active_connection_id.clone()?; let conn_id = dbc_state::connection_scope_key(&conn_id, self.active_database.as_deref());`
  - `save_view_prefs_for_grid`: `let Some(conn_id) = self.active_connection_id.clone() else { return };` → same wrap with `connection_scope_key`.

- [ ] **Step 6: Run everything to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-state` (all new + ALL existing tests — the existing `conn_identity_matches_tests`, script/CSV dispatch tests etc. keep passing untouched because `conn_identity_matches` itself did not change; zero warnings).
- [ ] **Step 7: Commit** — `feat(ui): (connection, database) identity core — conn_identity_for, resolve_active, spec_for_database, legacy store keys (sidebar T3)`.

---

### Task 4 (T4): schema_tree.rs — ADDITIVE multi-root state layer + `flatten_sidebar`

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs`
- Modify: `crates/dbc-ui/src/connections_ui.rs` — exactly two one-line visibility edits: `fn engine_label` → `pub(crate) fn engine_label`, `fn connect_needs_vault_prompt` → `pub(crate) fn connect_needs_vault_prompt` (the latter is consumed in T5; landing it here keeps T5 out of merge conflicts with itself)

**ON-flip discipline:** everything in this task is ADDITIVE. The existing `flatten`, `NodeId`, `TreeEvent`, the `SchemaTree` entity and every test keep working untouched — the old sidebar still drives the UI after this task. T5 flips.

**Interfaces (produced; consumed by T5, T6):**

```rust
// New pure types — NodeId itself is UNCHANGED (path-stable within one
// database); scope travels ALONGSIDE it in the wrapper, not inside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SidebarRow {
    Folder { path: Vec<String> },
    Connection { conn_id: String },
    Database { conn_id: String, db: String },
    Inner { conn_id: String, db: String, node: NodeId },
    /// Pinned active-context rows: AdminRoot, FavouriteSection, Favourite.
    Pinned(NodeId),
    /// "Načítám…"/error/truncation rows. `retry` = a click re-emits the
    /// Load event (db == None → LoadDatabases, Some → LoadSchema).
    Notice { conn_id: String, db: Option<String>, text: String, retry: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OuterId {
    Folder(Vec<String>),
    Connection(String),
    Database(String, String),
    Favourites,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveScope {
    pub conn_id: String,
    pub db: String,          // the EFFECTIVE database (spec-level string)
    pub default_db: String,  // the saved config's database
}

pub struct ConnNode { pub id: String, pub dbs: DbListState }
pub enum DbListState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { dbs: Vec<DbNode>, truncated: bool },
}
pub struct DbNode { pub name: String, pub is_default: bool, pub schema: DbSchemaState }
pub enum DbSchemaState {
    NotLoaded,
    Loading { generation: u64, prev_expanded: HashSet<NodeId> },
    Error(String),
    Loaded { snapshot: SchemaSnapshot, expanded: HashSet<NodeId> },
}

pub type SidebarFlatRow = (SidebarRow, usize, String, bool); // (row, depth, label, expandable)

pub const LOADED_SNAPSHOT_CAP: usize = 8;

pub fn flatten_schema(snapshot: &SchemaSnapshot, expanded: &HashSet<NodeId>, filter: &str) -> Vec<FlatNode>;
pub fn flatten_sidebar(
    grouped: &crate::connections_ui::GroupedConnections,
    states: &HashMap<String, ConnNode>,       // by connection id; missing id ⇒ NotLoaded
    cli: Option<(&str, &DbSchemaState)>,      // (label, slot) — the synthetic CLI root
    outer_expanded: &HashSet<OuterId>,
    filter: &str,
    active: Option<&ActiveScope>,
    favourites: &[FavouriteObject],
    admin: AdminEntry,
) -> Vec<SidebarFlatRow>;
pub fn display_db_name(engine: Engine, db: &str) -> String;   // file stem for file engines
pub fn favourite_in_scope(f: &FavouriteObject, scope: &ActiveScope) -> bool;
pub fn row_in_active_scope(row: &SidebarRow, scope: Option<&ActiveScope>) -> bool; // gates ★/⊞/⇪/DDL affordances (T5 render)
pub fn begin_schema_load(slot: &mut DbSchemaState, generation: u64);
pub fn apply_schema_result(slot: &mut DbSchemaState, my_gen: u64, result: Result<SchemaSnapshot, String>);
pub fn begin_db_list_load(node: &mut ConnNode, generation: u64);
pub fn apply_db_list_result(node: &mut ConnNode, my_gen: u64, result: Result<(Vec<String>, bool), String>, default_db: &str);
pub fn touch_and_evict(states: &mut HashMap<String, ConnNode>, lru: &mut Vec<(String, String)>, touched: (String, String), active: Option<&(String, String)>);
```

- [ ] **Step 1: Extract `flatten_schema`.** Move the body of `flatten` AFTER the admin-row push and `emit_favourites_section` call into a new `pub fn flatten_schema(snapshot, expanded, filter) -> Vec<FlatNode>` (the schema-key collection, `single_implicit` decision, and the per-schema `emit_sections` loop — verbatim). `flatten` becomes a wrapper with its exact current signature that pushes the admin row, calls `emit_favourites_section`, then extends with `flatten_schema(..)` — **all existing `flatten_tests` must pass unchanged.** Run `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui flatten` to prove it before continuing.

- [ ] **Step 2: Write the failing tests** for the new layer, in a new `mod sidebar_tests` (same pure, GPUI-free style as `flatten_tests` — reuse its snapshot-fixture helpers by making them `pub(crate)` within the test module or duplicating the small `SchemaSnapshot` builders):

```rust
#[cfg(test)]
mod sidebar_tests {
    use super::*;
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
                indexes: vec![], ddl: None,
            }],
            routines: vec![], triggers: vec![], sequences: vec![],
        }
    }

    fn loaded_states(conn_id: &str, db: &str) -> HashMap<String, ConnNode> {
        let mut m = HashMap::new();
        m.insert(conn_id.to_string(), ConnNode {
            id: conn_id.to_string(),
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
        outer.insert(OuterId::Folder(vec!["work".into()]));
        outer.insert(OuterId::Connection("c1".into()));
        outer.insert(OuterId::Database("c1".into(), "sales".into()));
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", "sales"), None,
            &outer, "", None, &[], AdminEntry::Hidden);
        // folder(0) → connection(1) → database(2) → spliced schema rows (3+)
        assert!(matches!(&rows[0], (SidebarRow::Folder { path }, 0, _, true) if path == &vec!["work".to_string()]));
        assert!(matches!(&rows[1], (SidebarRow::Connection { conn_id }, 1, _, true) if conn_id == "c1"));
        assert!(matches!(&rows[2], (SidebarRow::Database { conn_id, db }, 2, label, true)
            if conn_id == "c1" && db == "sales" && label.contains("(výchozí)")));
        assert!(matches!(&rows[3], (SidebarRow::Inner { conn_id, db, node: NodeId::Schema(_) }, 3, _, _)
            if conn_id == "c1" && db == "sales"));
    }

    #[test]
    fn loose_connections_sit_at_depth_zero() {
        let conns = vec![conn_cfg("c1", "loose", &[], Engine::Postgres, "db")];
        let rows = flatten_sidebar(&grouped(&conns), &HashMap::new(), None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden);
        assert!(matches!(&rows[0], (SidebarRow::Connection { .. }, 0, _, true)));
    }

    #[test]
    fn collapsed_connection_hides_children_but_keeps_cache() {
        let conns = vec![conn_cfg("c1", "prod", &[], Engine::Postgres, "sales")];
        let states = loaded_states("c1", "sales");
        // NOT in outer_expanded → only the connection row renders; the
        // Loaded cache is untouched (re-expand is instant by construction).
        let rows = flatten_sidebar(&grouped(&conns), &states, None,
            &HashSet::new(), "", None, &[], AdminEntry::Hidden);
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
            states.insert("c1".into(), ConnNode { id: "c1".into(), dbs: state });
            let rows = flatten_sidebar(&grouped(&conns), &states, None,
                &outer, "", None, &[], AdminEntry::Hidden);
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
            &outer, "", None, &[], AdminEntry::Hidden);
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
            None, &outer, "", None, &[], AdminEntry::Hidden);
        assert!(matches!(&rows[1], (SidebarRow::Database { db, .. }, 1, label, true)
            if db == r"D:\data\analytics.duckdb" && label.starts_with("analytics")));
    }

    #[test]
    fn cli_root_splices_schema_directly_without_a_database_level() {
        // Resolved deviation 12: no db switching on the CLI path ⇒ no dead
        // switch-target row.
        let slot = DbSchemaState::Loaded { snapshot: snap(), expanded: HashSet::new() };
        let mut outer = HashSet::new();
        outer.insert(OuterId::Connection(crate::CLI_CONN_IDENTITY.to_string()));
        let rows = flatten_sidebar(&grouped(&[]), &HashMap::new(),
            Some(("postgres://localhost/x", &slot)), &outer, "", None, &[], AdminEntry::Hidden);
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
            &outer, "", Some(&scope), &favs, AdminEntry::Enabled);
        assert!(matches!(&rows[0], (SidebarRow::Pinned(NodeId::AdminRoot), 0, _, false)));
        // Only the default-db favourite is in scope (database: None == default):
        assert!(matches!(&rows[1], (SidebarRow::Pinned(NodeId::FavouriteSection), 0, label, true)
            if label == "Oblíbené (1)"));
        assert!(matches!(&rows[2], (SidebarRow::Pinned(NodeId::Favourite(..)), 1, label, false)
            if label == "public.orders"));
    }

    /// Design §5 row 1's REQUIRED "active-scope gating of icon
    /// affordances" test — the pure predicate T5's render uses to decide
    /// whether a row gets the ★/⊞/⇪ icons and DDL-header enablement.
    /// Cross-context ambient actions must simply not exist.
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
        assert!(!favourite_in_scope(&f(None), &scope));            // default (= sales) ≠ inventory
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
            &outer, "orders", None, &[], AdminEntry::Hidden);
        assert!(rows.iter().any(|r| matches!(&r.0, SidebarRow::Connection { conn_id } if conn_id == "c1")));
        assert!(!rows.iter().any(|r| matches!(&r.0, SidebarRow::Connection { conn_id } if conn_id == "c2")));
        assert!(rows.iter().any(|r| matches!(&r.0, SidebarRow::Inner { node: NodeId::Table(_, t), .. } if t == "orders")));
        // A filter matching a connection's own NAME keeps its row visible:
        let rows = flatten_sidebar(&grouped(&conns), &loaded_states("c1", "sales"), None,
            &outer, "staging", None, &[], AdminEntry::Hidden);
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
        let mut node = ConnNode { id: "c1".into(), dbs: DbListState::NotLoaded };
        begin_db_list_load(&mut node, 3);
        apply_db_list_result(&mut node, 3,
            Ok((vec!["inventory".into(), "sales".into()], true)), "sales");
        let DbListState::Loaded { dbs, truncated } = &node.dbs else { panic!() };
        assert!(truncated);
        assert_eq!(dbs.iter().map(|d| (d.name.as_str(), d.is_default)).collect::<Vec<_>>(),
            vec![("inventory", false), ("sales", true)]);
        assert!(dbs.iter().all(|d| matches!(d.schema, DbSchemaState::NotLoaded)));
    }

    #[test]
    fn lru_evicts_oldest_loaded_but_never_the_active_slot() {
        let mut states: HashMap<String, ConnNode> = HashMap::new();
        let mut lru: Vec<(String, String)> = Vec::new();
        // Load CAP + 2 slots on one connection.
        let db_names: Vec<String> = (0..LOADED_SNAPSHOT_CAP + 2).map(|i| format!("db{i}")).collect();
        states.insert("c1".into(), ConnNode {
            id: "c1".into(),
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
}
```

- [ ] **Step 3: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui sidebar_tests` (compile errors on all new names).
- [ ] **Step 4: Implement.** All in `schema_tree.rs` (plus the two `pub(crate)` one-liners in connections_ui.rs). Key bodies:

```rust
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
        SidebarRow::Folder { .. }
        | SidebarRow::Connection { .. }
        | SidebarRow::Database { .. }
        | SidebarRow::Notice { .. } => false,
    }
}

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
            dbs: names.into_iter().map(|name| DbNode {
                is_default: name == default_db,
                schema: DbSchemaState::NotLoaded,
                name,
            }).collect(),
            truncated,
        },
    };
}

/// Design §6: bounded snapshot cache. Push `touched` to the back of `lru`
/// (dedup), then evict `Loaded` slots beyond `LOADED_SNAPSHOT_CAP` back to
/// `NotLoaded`, oldest first, NEVER the active slot. A `SchemaSnapshot`
/// can be thousands of objects; eight covers real cross-db work while
/// bounding memory on a hoarder server.
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
```

  `flatten_sidebar` — a plain loop, **no recursion anywhere** (design §6: outer loop + the existing iterative `emit_*` helpers; folder-path length is only an indent multiplier, never a recursion depth):

```rust
pub fn flatten_sidebar(
    grouped: &crate::connections_ui::GroupedConnections,
    states: &HashMap<String, ConnNode>,
    cli: Option<(&str, &DbSchemaState)>,
    outer_expanded: &HashSet<OuterId>,
    filter: &str,
    active: Option<&ActiveScope>,
    favourites: &[FavouriteObject],
    admin: AdminEntry,
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

    // CLI synthetic root (design §3.4 / resolved deviation 12).
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
    let mut emit_conn = |out: &mut Vec<SidebarFlatRow>, c: &ConnectionConfig, depth: usize| {
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
                    SidebarRow::Notice { conn_id: c.id.clone(), db: None, text: "Načítám databáze…".into(), retry: false },
                    depth + 1, "Načítám databáze…".into(), false,
                )),
                DbListState::Error(e) => out.push((
                    SidebarRow::Notice { conn_id: c.id.clone(), db: None, text: format!("error: {e}"), retry: true },
                    depth + 1, format!("error: {e}"), false,
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
                            depth + 1, db_label.clone(), true,
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
                        let text = "… zobrazeno prvních 2000 databází — použijte výchozí databázi nebo filtr".to_string();
                        out.push((
                            SidebarRow::Notice { conn_id: c.id.clone(), db: None, text: text.clone(), retry: false },
                            depth + 1, text, false,
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
        // A folder is "collapsed" when its OuterId is IN the set (folders
        // default to expanded — the pre-rework dropdown showed everything;
        // storing the exception keeps old sessions looking unchanged).
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
            SidebarRow::Notice { conn_id: conn_id.to_string(), db: Some(db.to_string()), text: "Načítám schéma…".into(), retry: false },
            base_depth, "Načítám schéma…".into(), false,
        )),
        DbSchemaState::Error(e) => out.push((
            SidebarRow::Notice { conn_id: conn_id.to_string(), db: Some(db.to_string()), text: format!("error: {e}"), retry: true },
            base_depth, format!("error: {e}"), false,
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
```

  Note on the folder-collapse polarity: `OuterId::Folder` in the set means **collapsed** (inverted vs Connection/Database, where presence means expanded) — folders default OPEN, connections/databases default CLOSED (lazy). Doc-comment this asymmetry on `OuterId`. Note `flatten_schema` takes the already-lowercased filter here — match the extraction from Step 1 (if `flatten_schema` lowercases internally, pass `filter` instead; keep ONE convention and pin it in a test).

- [ ] **Step 5: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (new `sidebar_tests` AND the untouched `flatten_tests` both pass; zero warnings — the new pub items are exercised by tests, so no dead-code allows are needed).
- [ ] **Step 6: Commit** — `feat(tree): additive multi-root state layer — SidebarRow, per-(conn,db) slots, flatten_sidebar, LRU cap (sidebar T4)`.

---

### Task 5 (T5): THE FLIP — entity + render + scoped events + main.rs wiring + `switch_to_database` (SOLO)

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs` (entity fields/methods, render, `TreeEvent`)
- Modify: `crates/dbc-ui/src/main.rs` (event handlers, slot fetches, switch, queue)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`PendingAfterUnlock` variants + `resume_pending`/cancel arms, `switch_to_connection` → wrapper)

This is the sweep task of the phase (G16-T3 precedent). After it, the multi-root sidebar IS the UI. Method: land the entity/event changes first, then let the compiler enumerate every main.rs site that used the old single-root API — the error list is the checklist.

**Interfaces (produced; consumed by T6, T7):**

```rust
// schema_tree.rs — TreeEvent after this task:
pub enum TreeEvent {
    /// WIDENED (design §5 row 1): carries the scope of the row that
    /// emitted it, so main.rs can switch-then-open across contexts.
    OpenPreview { conn_id: String, db: String, schema: Option<String>, table: String },
    OpenDdl { title: String, ddl: String },          // unchanged (local text; inert stamp)
    RefreshRequested,                                // unchanged (targets the ACTIVE slot)
    ToggleFavourite(FavouriteObject),                // unchanged shape; db stamped by favourite_object_for
    OpenErDiagram { schema: Option<String> },        // unchanged (icon renders on active-scope rows only)
    ImportCsv { schema: Option<String>, table: String }, // unchanged (same gating)
    OpenAdmin,                                       // unchanged
    LoadDatabases { conn_id: String },               // NEW — expand/retry a Connection row
    LoadSchema { conn_id: String, db: String },      // NEW — expand/retry a Database row
    SwitchToDatabase { conn_id: String, db: Option<String> }, // NEW — double-click; None = default
}

impl SchemaTree {
    pub fn sync_connections(&mut self, grouped: connections_ui::GroupedConnections, cx: &mut Context<Self>);
    pub fn set_active_scope(&mut self, scope: Option<ActiveScope>, cx: &mut Context<Self>);
    pub fn set_cli(&mut self, url: Option<String>, cx: &mut Context<Self>);
    pub fn begin_db_list(&mut self, conn_id: &str, generation: u64, cx: &mut Context<Self>);
    pub fn finish_db_list(&mut self, conn_id: &str, generation: u64,
        result: Result<(Vec<String>, bool), String>, default_db: &str, cx: &mut Context<Self>);
    pub fn begin_schema(&mut self, conn_id: &str, db: &str, generation: u64, cx: &mut Context<Self>); // "cli"/"" = CLI slot
    pub fn finish_schema(&mut self, conn_id: &str, db: &str, generation: u64,
        result: Result<SchemaSnapshot, String>, cx: &mut Context<Self>);
    pub fn collapse_connection(&mut self, conn_id: &str, cx: &mut Context<Self>); // vault-prompt cancel path
    pub fn snapshot(&self) -> Option<&SchemaSnapshot>; // NOW: the ACTIVE slot's snapshot (CLI slot when scope None + cli set) — signature unchanged, so fk/palette/autocomplete/admin call sites in main.rs compile untouched
    pub fn db_list_for(&self, conn_id: &str) -> Option<(&[DbNode], bool)>; // for T6's compare picker
}

// connections_ui.rs
pub enum PendingAfterUnlock {
    Connect(String),
    ExpandConnection(String),                        // NEW — resume LoadDatabases
    LoadDbSchema { conn_id: String, db: String },    // NEW — resume LoadSchema (deviation 9)
    SwitchDatabase { conn_id: String, db: Option<String> }, // NEW — resume the switch
    SaveConnection(Box<ConnectionFormData>),
    TestConnection(Box<ConnectionDialogUi>),
    Nothing,
}

// main.rs
impl AppView {
    pub(crate) fn switch_to_database(&mut self, id: &str, db: Option<String>, cx: &mut Context<Self>);
    fn start_db_list_fetch(&mut self, conn_id: String, cx: &mut Context<Self>);
    fn start_schema_slot_fetch(&mut self, conn_id: String, db: String, cx: &mut Context<Self>);
    fn scope_is_active(&self, conn_id: &str, db: &str) -> bool;
}
enum PendingTreeAction { OpenPreview { schema: Option<String>, table: String } }
// new AppView fields: sidebar_fetch_generation: u64, pending_after_switch: Option<PendingTreeAction>
// DELETED: schema_tree_connection_key, schema_fetch_generation, trigger_schema_fetch,
//          SchemaTree::{set_loading, set_snapshot, set_error, clear} (single-root lifecycle)
```

- [ ] **Step 1: Entity rework (schema_tree.rs).** Replace `SchemaTree`'s single-root fields (`snapshot`, `loading`, `error`) with:

```rust
    grouped: crate::connections_ui::GroupedConnections, // pushed by main.rs; the tree never owns a second copy long-term — re-synced on every config change
    conns: std::collections::HashMap<String, ConnNode>,
    lru: Vec<(String, String)>,
    outer_expanded: HashSet<OuterId>,
    active_scope: Option<ActiveScope>,
    cli_url: Option<String>,
    cli_slot: DbSchemaState,
    selected: Option<SidebarRow>,   // was Option<NodeId>
    // kept: filter, focus_handle, editor_focus, favourites, read_only, admin_entry
    // DELETED: active_connection_id (subsumed by active_scope.conn_id)
```

  Method bodies delegate to T4's pure fns; the two finish methods also drop stale results and drive the LRU:

```rust
    pub fn finish_db_list(
        &mut self,
        conn_id: &str,
        generation: u64,
        result: Result<(Vec<String>, bool), String>,
        default_db: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(node) = self.conns.get_mut(conn_id) {
            apply_db_list_result(node, generation, result, default_db);
            cx.notify();
        }
    }

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
        let Some(slot) = self.slot_mut(conn_id, db) else { return };
        apply_schema_result(slot, generation, result);
        let active = self.active_scope.as_ref().map(|s| (s.conn_id.clone(), s.db.clone()));
        touch_and_evict(&mut self.conns, &mut self.lru,
            (conn_id.to_string(), db.to_string()), active.as_ref());
        cx.notify();
    }

    fn slot_mut(&mut self, conn_id: &str, db: &str) -> Option<&mut DbSchemaState> {
        let DbListState::Loaded { dbs, .. } = &mut self.conns.get_mut(conn_id)?.dbs else { return None };
        dbs.iter_mut().find(|d| d.name == db).map(|d| &mut d.schema)
    }

    pub fn begin_db_list(&mut self, conn_id: &str, generation: u64, cx: &mut Context<Self>) {
        if let Some(node) = self.conns.get_mut(conn_id) {
            begin_db_list_load(node, generation);
            cx.notify();
        }
    }

    pub fn begin_schema(&mut self, conn_id: &str, db: &str, generation: u64, cx: &mut Context<Self>) {
        if conn_id == crate::CLI_CONN_IDENTITY {
            begin_schema_load(&mut self.cli_slot, generation);
        } else if let Some(slot) = self.slot_mut(conn_id, db) {
            begin_schema_load(slot, generation);
        } else {
            return; // no such slot (list not loaded / db vanished) — the fetch result will be dropped by the generation check anyway
        }
        cx.notify();
    }

    /// Vault-prompt cancel path (design §1.3): the user declined — collapse
    /// the row back; its state stays NotLoaded, no error row.
    pub fn collapse_connection(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        self.outer_expanded.remove(&OuterId::Connection(conn_id.to_string()));
        cx.notify();
    }
```

  `sync_connections` keeps existing `ConnNode` states for still-present ids, inserts `NotLoaded` for new ids, drops removed ids (and their lru entries). `snapshot()`:

```rust
    /// The ACTIVE context's snapshot — same signature as the single-root
    /// era so every main.rs consumer (fk lookups, editable detection,
    /// palette items, autocomplete, admin schema seed) compiles untouched.
    pub fn snapshot(&self) -> Option<&SchemaSnapshot> {
        if let Some(scope) = &self.active_scope {
            let DbListState::Loaded { dbs, .. } = &self.conns.get(&scope.conn_id)?.dbs else { return None };
            let d = dbs.iter().find(|d| d.name == scope.db)?;
            let DbSchemaState::Loaded { snapshot, .. } = &d.schema else { return None };
            return Some(snapshot);
        }
        if self.cli_url.is_some() {
            if let DbSchemaState::Loaded { snapshot, .. } = &self.cli_slot {
                return Some(snapshot);
            }
        }
        None
    }
```

  `favourite_object_for` reworks over `SidebarRow` (only ACTIVE-scope `Inner` rows and `Pinned(Favourite)` rows yield one) and stamps the database:

```rust
    fn favourite_object_for(&self, row: &SidebarRow) -> Option<FavouriteObject> {
        let scope = self.active_scope.as_ref()?;
        let node = match row {
            SidebarRow::Inner { conn_id, db, node }
                if conn_id == &scope.conn_id && db == &scope.db => node,
            SidebarRow::Pinned(node @ NodeId::Favourite(..)) => node,
            _ => return None,
        };
        // Non-default db → Some(db); default → None (design §5 row 9's
        // back-compat rule — existing favourites keep meaning what they meant).
        let database = (scope.db != scope.default_db).then(|| scope.db.clone());
        // ... existing per-NodeId body (Table kind lookup now against
        // self.snapshot() — the ACTIVE slot), with `database` added to
        // every FavouriteObject literal and connection_id = scope.conn_id ...
    }
```

  `handle_double_click` moves to `SidebarRow`:

```rust
    fn handle_double_click(&mut self, row: &SidebarRow, cx: &mut Context<Self>) {
        self.selected = Some(row.clone());
        match row {
            // Design §2.1: double-click on a Database row switches; on a
            // Connection row switches to the DEFAULT db (dropdown parity).
            // Expanding (chevron) never switches — browsing ≠ switching.
            SidebarRow::Database { conn_id, db } => cx.emit(TreeEvent::SwitchToDatabase {
                conn_id: conn_id.clone(), db: Some(db.clone()),
            }),
            SidebarRow::Connection { conn_id } if conn_id != crate::CLI_CONN_IDENTITY => {
                cx.emit(TreeEvent::SwitchToDatabase { conn_id: conn_id.clone(), db: None })
            }
            SidebarRow::Connection { .. } | SidebarRow::Folder { .. } => self.toggle_outer(row),
            SidebarRow::Inner { conn_id, db, node } => match node {
                NodeId::Table(schema, name) => {
                    let schema = if schema.is_empty() { None } else { Some(schema.clone()) };
                    cx.emit(TreeEvent::OpenPreview {
                        conn_id: conn_id.clone(), db: db.clone(),
                        schema, table: name.clone(),
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
                    let other = other.clone();
                    self.toggle_inner(&conn_id.clone(), &db.clone(), &other);
                }
            },
            // Pinned favourite rows keep the pre-flip semantics verbatim,
            // resolved against the ACTIVE slot's snapshot (`self.snapshot()`):
            // table/view → OpenPreview (with the active scope's conn/db),
            // routine/trigger → OpenDdl, sequence → no-op; the section
            // header toggles `OuterId::Favourites`; AdminRoot double-click
            // is a no-op (single click handles it, as today).
            SidebarRow::Pinned(node) => {
                if let (NodeId::Favourite(kind, schema, name), Some(scope)) =
                    (node, self.active_scope.clone())
                {
                    let schema_opt =
                        if schema.is_empty() { None } else { Some(schema.clone()) };
                    match kind.as_str() {
                        "table" | "view" => cx.emit(TreeEvent::OpenPreview {
                            conn_id: scope.conn_id, db: scope.db,
                            schema: schema_opt, table: name.clone(),
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
        }
        cx.notify();
    }
```

  Supporting entity helpers (replace the old snapshot-global `find_routine_ddl`/`find_trigger_ddl`/`toggle_expand`):

```rust
    /// Immutable sibling of `slot_mut` (CLI slot for the "cli"/"" pair).
    fn snapshot_for(&self, conn_id: &str, db: &str) -> Option<&SchemaSnapshot> {
        let slot = if conn_id == crate::CLI_CONN_IDENTITY {
            &self.cli_slot
        } else {
            let DbListState::Loaded { dbs, .. } = &self.conns.get(conn_id)?.dbs else { return None };
            &dbs.iter().find(|d| d.name == db)?.schema
        };
        match slot {
            DbSchemaState::Loaded { snapshot, .. } => Some(snapshot),
            _ => None,
        }
    }

    fn find_routine_ddl_in(&self, conn_id: &str, db: &str, schema: &str, name: &str) -> Option<String> {
        self.snapshot_for(conn_id, db)?
            .routines.iter()
            .find(|r| r.name == name && schema_key_string(&r.schema) == schema)
            .and_then(|r| r.ddl.clone())
    }

    fn find_trigger_ddl_in(&self, conn_id: &str, db: &str, schema: &str, name: &str) -> Option<String> {
        self.snapshot_for(conn_id, db)?
            .triggers.iter()
            .find(|t| t.name == name && schema_key_string(&t.schema) == schema)
            .and_then(|t| t.ddl.clone())
    }

    /// Chevron/double-click on an Inner row: toggles the NodeId in ITS
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
    /// open — see OuterId's doc comment); everything else presence = open.
    fn toggle_outer(&mut self, row: &SidebarRow) {
        let id = match row {
            SidebarRow::Folder { path } => OuterId::Folder(path.clone()),
            SidebarRow::Connection { conn_id } => OuterId::Connection(conn_id.clone()),
            SidebarRow::Database { conn_id, db } => OuterId::Database(conn_id.clone(), db.clone()),
            SidebarRow::Pinned(NodeId::FavouriteSection) => OuterId::Favourites,
            _ => return,
        };
        if !self.outer_expanded.remove(&id) {
            self.outer_expanded.insert(id);
        }
    }
```

- [ ] **Step 2: Render rework (schema_tree.rs).** `render` computes rows via `flatten_sidebar(&self.grouped, &self.conns, cli, &self.outer_expanded, &self.filter, self.active_scope.as_ref(), &self.favourites, self.admin_entry)` where `cli = self.cli_url.as_deref().map(|u| (u, &self.cli_slot))`. Reuse the existing `uniform_list` body with these changes:
  - Chevron click: `Folder`/`Connection`/`Database`/`Pinned(FavouriteSection)` toggle their `OuterId` (folders inverted — presence = collapsed); a `Connection` expand with `dbs` `NotLoaded`/`Error` ALSO emits `LoadDatabases`; a `Database` expand with schema `NotLoaded`/`Error` ALSO emits `LoadSchema`; `Inner` chevrons toggle the row's slot-local `expanded` set (via a small `toggle_inner(conn_id, db, node)` helper over `slot_mut`).
  - Row click: single = select (`Notice { retry: true }` rows instead RE-EMIT their Load event — `db: None` → `LoadDatabases`, `Some` → `LoadSchema`); double = `handle_double_click`.
  - **Honest indicators (design §1.4):** `Connection` rows get a leading `●` in `cx.theme().accent` when `active_scope.conn_id` matches, else `○` in `text_disabled`; `Database` rows get `●` + `bg_selected`-family emphasis when `(conn_id, db)` IS the active scope. There is deliberately NO green/red connected lamp — doc-comment: *"the runner is per-operation (design fact 0.1); the two honest indicators are active context (●) and metadata cached (children present)"*.
  - **Icon gating (design §5 row 1):** the ★, ⊞, ⇪ icons and DDL-header enablement render ONLY when the row is `Pinned(..)` or an `Inner` row whose `(conn_id, db)` equals the active scope — cross-context ambient actions don't exist. `Notice` text rows use `text_muted`, errors `danger`.
  - `selected_table()` (DDL header button) matches only `Some(SidebarRow::Inner { conn_id, db, node: NodeId::Table(..) })` at the active scope, looked up in `snapshot()`.
  - The `loading`/`error`/`"Bez připojení"` whole-panel states are GONE — with no connections and no CLI url the list is empty; render the old `"Bez připojení"` div only when `grouped` is empty AND `cli_url` is `None`.

- [ ] **Step 3: `PendingAfterUnlock` variants (connections_ui.rs).** Add the three variants from Interfaces. `resume_pending` gains (compiler-forced):

```rust
            PendingAfterUnlock::ExpandConnection(id) => self.start_db_list_fetch(id, cx),
            PendingAfterUnlock::LoadDbSchema { conn_id, db } => self.start_schema_slot_fetch(conn_id, db, cx),
            PendingAfterUnlock::SwitchDatabase { conn_id, db } => self.switch_to_database(&conn_id, db, cx),
```

  `cancel_master_password_prompt` gains one arm (design §1.3: cancel = the user declined; collapse back, no error state):

```rust
            Some(ModalState::MasterPasswordPrompt { pending: PendingAfterUnlock::ExpandConnection(id), .. }) => {
                self.modal = None;
                self.tree.update(cx, |t, cx| t.collapse_connection(&id, cx));
            }
```

  (`LoadDbSchema`/`SwitchDatabase` cancels take the default `_ => self.modal = None` arm — the db row simply stays `NotLoaded`/unswitched.)

  `switch_to_connection`'s body becomes the delegating wrapper (resolved deviation 10) — dropdown, palette and `PendingAfterUnlock::Connect` all flow through the new path with default-db semantics:

```rust
    pub(crate) fn switch_to_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.switch_to_database(id, None, cx);
    }
```

- [ ] **Step 4: `switch_to_database` + queue + admin confirm (main.rs).** New fields on `AppView` (+ init in `main()`): `sidebar_fetch_generation: u64` (replaces `schema_fetch_generation`), `pending_after_switch: Option<PendingTreeAction>`.

```rust
    /// Design §2.2: THE context switch. `db == None` targets the saved
    /// default database (dropdown/palette/tree-connection-row semantics);
    /// `Some(db)` a tree-selected one. Success is the ONLY writer of
    /// `active_database`. A failed test_connect leaves the previous
    /// context untouched — same contract as the pre-rework switch.
    pub(crate) fn switch_to_database(&mut self, id: &str, db: Option<String>, cx: &mut Context<Self>) {
        self.cancel_active_backup_if_running();
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else { return };
        // Canonical spelling: explicitly picking the default == None.
        let db = db.filter(|d| d != &cfg.database);
        let target_identity = conn_identity_for(id, db.as_deref().unwrap_or(&cfg.database));
        if target_identity == self.current_conn_identity() {
            // Already there — still worth a re-validate? No: match the old
            // dropdown behaviour (clicking the active item re-tested).
            // Deliberate change: a no-op switch is a no-op; the ⟳ button
            // owns re-validation. Keeps double-click idempotent.
            return;
        }
        // Resolved deviation 11 (risk list "release-note + confirm"): a
        // context switch makes a dirty admin tab's staged edits
        // permanently inapplicable (the identity guard will refuse them
        // and the next admin open Replaces the tab) — confirm first.
        if self.discard_confirm.is_none() {
            if let Some(count) = self.dirty_admin_change_count(cx) {
                self.discard_confirm = Some(DiscardConfirmState {
                    change_count: count,
                    action: PendingDiscard::SwitchDatabase { conn_id: id.to_string(), db },
                });
                self.modal_needs_focus = true;
                cx.notify();
                return;
            }
        }
        // Vault gate (design §1.3/§4.4) — same three-boolean predicate as
        // the dropdown's.
        let needs_secret = !connections_ui::engine_is_file_based(cfg.engine);
        if connections_ui::connect_needs_vault_prompt(needs_secret, self.vault.is_some(), Vault::exists(&self.vault_path)) {
            self.open_vault_prompt(
                connections_ui::PendingAfterUnlock::SwitchDatabase { conn_id: id.to_string(), db },
                cx,
            );
            return;
        }
        let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
        let engine_lbl = connections_ui::engine_label(cfg.engine);
        let effective = db.clone().unwrap_or_else(|| cfg.database.clone());
        let spec = spec_for_database(&cfg, &effective, secret);
        let target_id = cfg.id.clone();
        self.dropdown_open = false;
        self.status = "connecting…".into();
        self.switch_generation += 1;
        let my_generation = self.switch_generation;
        cx.notify();
        let rx = self.runner.test_connect(spec);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                if view.switch_generation != my_generation {
                    return; // superseded — last-dispatched wins
                }
                match result {
                    Ok(Ok(())) => {
                        view.status = format!("Připojeno ({engine_lbl})");
                        view.active_connection_id = Some(target_id.clone());
                        view.active_database = db.clone();
                        view.conn_url = None;
                        view.close_autocomplete(cx);
                        view.push_active_scope_to_tree(cx);
                        view.start_schema_slot_fetch(target_id.clone(), effective.clone(), cx);
                        if let Some(action) = view.pending_after_switch.take() {
                            match action {
                                PendingTreeAction::OpenPreview { schema, table } => {
                                    view.open_preview_for_active(schema, table, cx);
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        view.status = format!("error: {e}");
                        view.pending_after_switch = None; // failure clears the queue
                    }
                    Err(_) => {
                        view.status = "error: connect zrušen".into();
                        view.pending_after_switch = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
```

  Helpers (all main.rs):

```rust
    /// Some(change_count) when an open admin tab is stamped with the
    /// CURRENT identity and has staged edits (roles/memberships/matrix).
    fn dirty_admin_change_count(&self, cx: &Context<Self>) -> Option<usize> {
        let current = self.current_conn_identity();
        self.tabs.iter().find_map(|t| match &t.content {
            TabContent::Admin { view } => {
                let p = view.read(cx);
                let n = p.change_count();
                (p.conn_identity() == current && n > 0).then_some(n)
            }
            _ => None,
        })
    }

    /// Opens the master-password prompt from a cx-only context (tree
    /// subscribe callbacks have no &mut Window) — deferred focus lands on
    /// the prompt's own input via the render-top hook (see Step 7).
    fn open_vault_prompt(&mut self, pending: connections_ui::PendingAfterUnlock, cx: &mut Context<Self>) {
        let input = cx.new(|cx| connections_ui::TextField::form_field(cx, "Heslo", true));
        self.modal = Some(connections_ui::ModalState::MasterPasswordPrompt { input, error: None, pending });
        self.dropdown_open = false;
        self.modal_needs_focus = true;
        cx.notify();
    }

    fn scope_is_active(&self, conn_id: &str, db: &str) -> bool {
        if conn_id == CLI_CONN_IDENTITY {
            return self.active_connection_id.is_none() && self.conn_url.is_some();
        }
        self.active_connection_id.as_deref() == Some(conn_id)
            && self.effective_database().as_deref() == Some(db)
    }

    fn push_active_scope_to_tree(&mut self, cx: &mut Context<Self>) {
        let scope = self.active_connection_id.as_ref().and_then(|id| {
            let cfg = self.config.connections.iter().find(|c| &c.id == id)?;
            Some(schema_tree::ActiveScope {
                conn_id: id.clone(),
                db: self.active_database.clone().unwrap_or_else(|| cfg.database.clone()),
                default_db: cfg.database.clone(),
            })
        });
        let cli = self.conn_url.clone();
        self.tree.update(cx, |t, cx| {
            t.set_active_scope(scope, cx);
            t.set_cli(cli, cx); // switch success sets conn_url = None → CLI root disappears (design §3.4)
        });
    }

    /// The old trigger_schema_fetch success-arm's M2-guarded admin-schema
    /// push, verbatim (AUDIT SITE — design §7 guard list: "only push into
    /// an admin panel whose OWN stamped identity still matches the
    /// CURRENTLY active connection").
    fn push_admin_schemas_if_matching(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.tree.read(cx).snapshot() else { return };
        let schemas = admin_panel::distinct_schemas(snapshot);
        if let Some(panel) = self.tabs.iter().find_map(|t| match &t.content {
            TabContent::Admin { view } => Some(view.clone()),
            _ => None,
        }) {
            let current_identity = self.current_conn_identity();
            if conn_identity_matches(panel.read(cx).conn_identity(), &current_identity) {
                panel.update(cx, |p, cx| p.set_schemas(schemas, cx));
            }
        }
    }
```

  `PendingDiscard` gains (and `on_discard_confirm_yes` handles) the confirmed switch — note it re-enters `switch_to_database`, which now finds `dirty_admin_change_count` still Some but `discard_confirm` just-taken... to avoid a loop, the Yes-arm bypasses the check by closing the dirty admin tab first (that IS the "drop staged edits" the user just confirmed, and it matches `AdminOpenDecision::Replace`'s existing posture):

```rust
    /// Resolved deviation 11: the user confirmed dropping the staged admin
    /// edits — close the dirty admin tab, then perform the switch.
    SwitchDatabase { conn_id: String, db: Option<String> },
```

```rust
            PendingDiscard::SwitchDatabase { conn_id, db } => {
                if let Some(id) = self.tabs.iter().find_map(|t| {
                    matches!(&t.content, TabContent::Admin { .. }).then_some(t.id)
                }) {
                    self.tabs.close(id);
                }
                self.switch_to_database(&conn_id, db, cx);
            }
```

  ("Zrušit" takes the existing no-op path — no switch happens.) The discard-confirm overlay's copy already renders `change_count`; no copy change needed.

- [ ] **Step 5: Slot fetch dispatchers (main.rs).**

```rust
    /// Expand of a Connection row (or its error-row retry / vault resume).
    /// Design §1.2: NOT eager — one bounded metadata fetch over one
    /// short-lived connection to the DEFAULT database; no other connection
    /// is touched, no schema is fetched yet.
    fn start_db_list_fetch(&mut self, conn_id: String, cx: &mut Context<Self>) {
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == conn_id).cloned() else { return };
        let needs_secret = !connections_ui::engine_is_file_based(cfg.engine);
        if connections_ui::connect_needs_vault_prompt(needs_secret, self.vault.is_some(), Vault::exists(&self.vault_path)) {
            self.open_vault_prompt(connections_ui::PendingAfterUnlock::ExpandConnection(conn_id), cx);
            return;
        }
        let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
        self.sidebar_fetch_generation += 1;
        let my_generation = self.sidebar_fetch_generation;
        let default_db = cfg.database.clone();
        self.tree.update(cx, |t, cx| t.begin_db_list(&conn_id, my_generation, cx));
        let rx = self.runner.fetch_database_list(spec_for_database(&cfg, &cfg.database, secret));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                let result = match result {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("výpis databází zrušen".to_string()),
                };
                view.tree.update(cx, |t, cx| {
                    t.finish_db_list(&conn_id, my_generation, result, &default_db, cx)
                });
            });
        })
        .detach();
    }

    /// Expand of a Database row / ⟳ refresh of the active slot / the
    /// switch success arm. CLI slot: conn_id == CLI_CONN_IDENTITY, db "".
    fn start_schema_slot_fetch(&mut self, conn_id: String, db: String, cx: &mut Context<Self>) {
        self.sidebar_fetch_generation += 1;
        let my_generation = self.sidebar_fetch_generation;
        let spec = if conn_id == CLI_CONN_IDENTITY {
            let Some(url) = self.conn_url.clone() else { return };
            ConnectSpec::Url(url)
        } else {
            let Some(cfg) = self.config.connections.iter().find(|c| c.id == conn_id).cloned() else { return };
            let needs_secret = !connections_ui::engine_is_file_based(cfg.engine);
            if connections_ui::connect_needs_vault_prompt(needs_secret, self.vault.is_some(), Vault::exists(&self.vault_path)) {
                // Design §4.4 + resolved deviation 9: the vault can lock
                // BETWEEN expanding a connection and expanding one of its
                // databases — never fetch with an empty secret fallback.
                self.open_vault_prompt(
                    connections_ui::PendingAfterUnlock::LoadDbSchema { conn_id, db },
                    cx,
                );
                return;
            }
            let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
            spec_for_database(&cfg, &db, secret)
        };
        self.tree.update(cx, |t, cx| t.begin_schema(&conn_id, &db, my_generation, cx));
        let rx = self.runner.fetch_schema(spec);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                let result = match result {
                    Ok(Ok(s)) => Ok(s),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("fetch zrušen".to_string()),
                };
                let ok = result.is_ok();
                view.tree.update(cx, |t, cx| t.finish_schema(&conn_id, &db, my_generation, result, cx));
                // The old trigger_schema_fetch success-arm side effects,
                // ACTIVE slot only:
                if ok && view.scope_is_active(&conn_id, &db) {
                    view.refresh_tree_context(cx);      // favourites + read_only + admin_entry push (Step 6)
                    view.push_admin_schemas_if_matching(cx); // M2 guard preserved verbatim (audit site!)
                    view.close_autocomplete(cx);
                }
            });
        })
        .detach();
    }
```

- [ ] **Step 6: The compile-error sweep (main.rs).** Delete `schema_tree_connection_key`, `schema_fetch_generation`, `trigger_schema_fetch`, and the old `SchemaTree::{set_loading, set_snapshot, set_error, clear}`. Build `-p dbc-ui` and fix every error against this checklist (every error must map to a row; an unmapped error means the sweep list was incomplete — fix with matching posture and record in the commit message):
  - `trigger_schema_fetch` call sites → `start_schema_slot_fetch` for the ACTIVE scope: the palette/tree `RefreshRequested` arms, the old switch success arm (now inside `switch_to_database`), and any startup fetch. For the CLI startup path: after constructing `AppView` with `conn_url: Some(url)`, call `tree.set_cli(Some(url))` + `start_schema_slot_fetch(CLI_CONN_IDENTITY.into(), String::new(), cx)` wherever the old initial fetch fired.
  - `on_tree_event`: `OpenPreview` arm gains the scope check + queue (design §5 row 1):

```rust
            TreeEvent::OpenPreview { conn_id, db, schema, table } => {
                if self.scope_is_active(conn_id, db) {
                    // ... the entire existing OpenPreview body, unchanged ...
                } else {
                    // Cross-context double-click: switch first, open after
                    // (one-shot; cleared on failure/supersede — §2.2).
                    self.pending_after_switch = Some(PendingTreeAction::OpenPreview {
                        schema: schema.clone(), table: table.clone(),
                    });
                    self.switch_to_database(conn_id, Some(db.clone()), cx);
                }
            }
            TreeEvent::LoadDatabases { conn_id } => self.start_db_list_fetch(conn_id.clone(), cx),
            TreeEvent::LoadSchema { conn_id, db } => {
                self.start_schema_slot_fetch(conn_id.clone(), db.clone(), cx)
            }
            TreeEvent::SwitchToDatabase { conn_id, db } => {
                self.switch_to_database(conn_id, db.clone(), cx)
            }
```

    Extract the existing OpenPreview body into `fn open_preview_for_active(&mut self, schema: Option<String>, table: String, cx)` so the queue replay (Step 4) and the active-scope arm share it — including its dirty-preview discard-confirm gate, which the queued replay must ALSO pass through (a queued open must never silently drop staged edits either).
  - `set_favourites` push sites → a consolidated `fn refresh_tree_context(&mut self, cx)` that pushes `set_favourites(self.config.favourite_objects.clone(), ..)` (drop the now-redundant `active_connection_id` param — the tree filters via `active_scope`; adjust `set_favourites`'s signature to take only the Vec), `set_read_only(self.active_read_only())`, `set_admin_entry(..)`, and `push_active_scope_to_tree`. Call it from: the fetch success arm (Step 5), the ★-toggle handler, and after every config mutation alongside the `refresh_grouped_cache` call sites (grep `refresh_grouped_cache(` — each site gains `self.tree.update(cx, |t, cx| t.sync_connections(self.grouped_cache.clone(), cx));` — connections added/renamed/deleted must re-sync the tree; also call both once at startup).
  - `ToggleFavourite` handler: unchanged logic (full-struct `toggle_favourite` now distinguishes databases via T1's test).
  - `SchemaTree::new` call in `main()`: construct with the new fields defaulted (`grouped: GroupedConnections::default()`, empty maps, `cli_slot: DbSchemaState::NotLoaded`, etc.), then the startup sync calls above.
- [ ] **Step 7: Deferred focus for the tree-opened vault prompt.** In `AppView::render`'s `modal_needs_focus` block, focus the prompt's own input when the open modal is input-owning:

```rust
        if self.modal_needs_focus {
            self.modal_needs_focus = false;
            if let Some(connections_ui::ModalState::MasterPasswordPrompt { input, .. }) = &self.modal {
                // Sidebar rework: the tree's expand/switch vault gate opens
                // this input-owning prompt from a cx-only subscribe callback
                // — focus its field, same end state as the window-having
                // openers (dropdown/test).
                let focus = input.focus_handle(cx);
                window.focus(&focus, cx);
            } else if self.modal.is_some() || self.discard_confirm.is_some() {
                window.focus(&self.modal_focus_handle, cx);
            }
        }
```

- [ ] **Step 8: Pure tests for the new decision logic** (this crate has no GPUI harness — pure decompositions only, existing house style). In `main.rs` tests:

```rust
#[cfg(test)]
mod switch_decision_tests {
    use super::*;

    #[test]
    fn db_choice_normalizes_default_to_none() {
        // The .filter(|d| d != &cfg.database) line in switch_to_database —
        // pinned so identity/store-key/label logic keeps ONE canonical
        // spelling for "the default database".
        let default = "sales".to_string();
        assert_eq!(Some("sales".to_string()).filter(|d| d != &default), None);
        assert_eq!(Some("inventory".to_string()).filter(|d| d != &default), Some("inventory".to_string()));
    }

    /// The queue is one-shot open-preview only (design §2.2): success
    /// replays it, failure clears it — both arms are in switch_to_database
    /// and structurally covered; this pins the enum stays single-variant
    /// (a second queued kind needs its own design pass).
    #[test]
    fn pending_tree_action_is_open_preview_only() {
        let a = PendingTreeAction::OpenPreview { schema: None, table: "t".into() };
        match a { PendingTreeAction::OpenPreview { .. } => {} }
    }
}
```

  In `schema_tree.rs` `sidebar_tests`, add entity-free coverage already written in T4; extend with a `finish_schema`-shaped stale-drop test if not already covered (it is — `stale_generation_results_are_dropped`).
- [ ] **Step 9: Run everything to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-mcp -p dbc-core -p dbc-state` (zero warnings; existing `flatten_tests` now target `flatten_schema` — delete the old `flatten` wrapper and rewire its tests: pinned-row assertions move to `sidebar_tests::pinned_admin_and_favourites_render_once_scoped_to_active_context`, schema-shape assertions call `flatten_schema`).
- [ ] **Step 10: Manual smoke** (the one visual gate): `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui` with a config containing ≥2 connections (one sqlite/duckdb file) — expand without switching, double-click a database, watch the ● move and the editor target follow, ⟳ refresh, speed-search.
- [ ] **Step 11: Commit** — `feat(ui): multi-root sidebar flip — scoped TreeEvents, per-slot fetches, switch_to_database with vault/admin gates (sidebar T5)`.

---

### Task 6 (T6): connections_ui.rs — top-bar label, dropdown demotion, compare db sub-pick

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs`
- Modify: `crates/dbc-ui/src/compare.rs` only if `CompareView` label construction lives there (labels are built in `confirm_compare_dialog` — expected: no compare.rs change)

**Design §2.5 (binding):** the dropdown is KEPT, demoted to status + quick default-db switch — it is the sole home of the 🗄/♻ backup/restore entry points and the manager affordances, and the palette's `Connection` items route through it. The tree is the ONLY UI that reaches non-default databases. `on_dropdown_item_click` needs no edit (it calls `switch_to_connection`, already the `switch_to_database(id, None)` wrapper since T5).

- [ ] **Step 1: Write the failing tests** (pure label fns, existing test-mod style):

```rust
#[cfg(test)]
mod top_bar_label_tests {
    use super::*;

    #[test]
    fn label_appends_db_segment_only_when_non_default() {
        assert_eq!(connection_label("prod", Engine::Postgres, None), "prod (pg)");
        assert_eq!(connection_label("prod", Engine::Postgres, Some("inventory")), "prod (pg) · inventory");
    }

    #[test]
    fn compare_side_label_appends_db() {
        assert_eq!(compare_side_label("prod", Engine::Postgres, None), "prod (pg)");
        assert_eq!(compare_side_label("prod", Engine::Mssql, Some("staging")), "prod (mssql) / staging");
    }
}
```

- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui top_bar_label` (fns undefined).
- [ ] **Step 3: Implement the labels.**

```rust
/// Design §2.5: „{name} ({engine}) · {db}" — the db segment renders only
/// for a NON-default active database (`active_db == None` = default).
pub(crate) fn connection_label(name: &str, engine: Engine, active_db: Option<&str>) -> String {
    match active_db {
        Some(db) => format!("{} ({}) · {}", name, engine_label(engine), db),
        None => format!("{} ({})", name, engine_label(engine)),
    }
}

/// Design §5 row 7: compare-side display label; `/` separator to match
/// `conn_name_for_identity`'s mismatch-text convention.
pub(crate) fn compare_side_label(name: &str, engine: Engine, db: Option<&str>) -> String {
    match db {
        Some(db) => format!("{} ({}) / {}", name, engine_label(engine), db),
        None => format!("{} ({})", name, engine_label(engine)),
    }
}
```

  `current_connection_label` body becomes:

```rust
    pub(crate) fn current_connection_label(&self) -> String {
        if let Some(id) = &self.active_connection_id {
            if let Some(c) = self.config.connections.iter().find(|c| &c.id == id) {
                return connection_label(&c.name, c.engine, self.active_database.as_deref());
            }
        }
        if let Some(url) = &self.conn_url {
            return url.clone();
        }
        "Bez připojení".to_string()
    }
```

- [ ] **Step 4: Connection-dialog relabel (design §8 — label only, config shape untouched).** In `render_connection_dialog_panel`, the `field_row("Databáze", ..)` label becomes engine-conditional: `if engine_is_file_based(ui.engine) { "Databáze" } else { "Databáze (výchozí)" }` — for server engines the field now names the DEFAULT database of a multi-database connection; for file engines "výchozí" would be meaningless (one file, one database) so the plain label stays alongside G16's existing file-path hint row. No test (render string in a GPUI panel — this crate has no render harness; the conditional is one expression).

- [ ] **Step 5: Compare dialog db sub-pick (design §5 row 7).** Mechanical widening, compiler-led:
  - `ModalState::CompareDialog` sides: `conn_a: Option<(String, Option<String>)>, conn_b: Option<(String, Option<String>)>` — `(connection id, db)`, `None` db = default. Every existing construction/match site updates (the compiler enumerates them, including the `modal_confirm_kind` arm — shape-only, no logic change — and the existing compare modal tests, which update their tuple shapes).
  - `select_compare_side(side, id: String, db: Option<String>, cx)` gains the db param; picker rows pass it.
  - `render_compare_dialog_panel`: each connection row (click → `(id, None)`) is followed by indented rows for its CACHED database list — the panel gains a `db_lists: Vec<(String, Vec<String>)>` parameter that `render_modal_overlay`'s CompareDialog arm builds at render time from `self.tree.read(cx).db_list_for(&id)` for each listed connection (**the dialog never triggers fetches** — design row 7: connections without a cached list offer only their default row). Skip the `is_default` entry (it IS the default row). Selected-side highlight compares the full `(id, db)` tuple.
  - `confirm_compare_dialog`: resolve both sides' cfgs, then for a `Some(db)` side swap ONCE up front — `let cfg_a = match &db_a { Some(db) => { let mut c = cfg_a; c.database = db.clone(); c } None => cfg_a };` — so the stored `CompareView::conn_a/conn_b` (used later by the data-diff leg) already carry the effective database and the existing `ConnectSpec::Config` construction below needs no change. Labels via `compare_side_label`. Same-connection-two-databases is now expressible — the flagship capability.
- [ ] **Step 6: Run everything to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (zero warnings; existing compare-modal tests updated for the tuple shape).
- [ ] **Step 7: Commit** — `feat(ui): top-bar db segment, dialog relabel, compare same-connection two-database pick (sidebar T6)`.

---

### Task 7 (T7): the identity-widening AUDIT — every stamp/guard site, no guard got weaker

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` (tests + one audit doc comment; NO behavior changes)

This task is the phase's security gate (design §7). It re-verifies, ON THE FINAL CODE, that all ~26 stamp sites and 9 guard sites either inherited the stricter semantics or were consciously edited — and pins the "no guard got weaker" claim with tests.

- [ ] **Step 1: Mechanical site census.** Run and record in the commit message:
  - `grep -n "current_conn_identity()" crates/dbc-ui/src/main.rs crates/dbc-ui/src/connections_ui.rs` — expected ≈26 non-test hits. EVERY hit must be a stamp (writes into a tab/panel/modal/progress state) or a guard's "current" side. Classify each against the design §7 table (by symbol): `run_query_with` capture → `ResultTab.conn_identity`; `run_many`/ad-hoc tabs; `start_script_pick` → `ModalState::ScriptRun` → script progress tab; `start_csv_import` → `ModalState::CsvImport` → CSV progress tab; `dispatch_plan_query`/`on_confirm_analyze_write` → Plan tabs; chart tab; `open_admin_tab`/`open_fresh_admin_tab` → AdminPanel + tab; monitor `preview_key` `"monitor:{identity}"` + tab; ER tab + DDL child; `ApplyDialogState.conn_identity` (both openers); tree-DDL text tab (inert); compare tab (inert); `store_scope_key` must NOT appear in this list (it deliberately uses the legacy key — T3). **Any hit not in the table is a finding**: classify it, add it to the audit comment, and if it is a guard, test it.
  - `grep -n "conn_identity_matches(" crates/dbc-ui/src/main.rs` — expected 9 non-test guard sites: `admin_open_decision` (via `==` on identity — verify it still compares full identities), script continuation + `script_run_dispatch_allowed` enforcement, CSV continuation + `csv_import_dispatch_allowed` enforcement, `fetch_admin_catalog_into`'s M2 gap, `on_open_apply_dialog`, `on_confirm_apply` backstop, `open_admin_apply_dialog`, the schema-push M2 guard (now in `start_schema_slot_fetch`'s success arm — T5 Step 5 preserved it), `render_apply_bar` dim-out.
  - `grep -n "ConnectSpec::Config" crates/dbc-ui/src/main.rs` — every constructor must be inside `resolve_active_from`, `spec_for_database`, or a compare/backup explicit-config path. Zero direct `active_connection_id`-based builds (T3's invariant).
- [ ] **Step 2: Write the guard-family tests** (`mod identity_audit_tests`, pure — mirrors the existing `script_run_dispatch_allowed` test style):

```rust
#[cfg(test)]
mod identity_audit_tests {
    use super::*;

    /// Design §7's headline fix, per guard family: a SAME-CONNECTION
    /// database switch invalidates every pending write captured against
    /// the previous database. Pre-phase identities (bare ids) passed all
    /// four of these — pinning that they now refuse.
    #[test]
    fn same_connection_db_switch_refuses_script_and_csv_dispatch() {
        let sales = conn_identity_for("conn-a", "sales");
        let inventory = conn_identity_for("conn-a", "inventory");
        assert!(!script_run_dispatch_allowed(&sales, &inventory));
        assert!(!csv_import_dispatch_allowed(&sales, &inventory));
        assert!(script_run_dispatch_allowed(&sales, &sales));
    }

    /// Apply flow (on_open_apply_dialog / on_confirm_apply backstop /
    /// render_apply_bar dim-out all route through conn_identity_matches).
    #[test]
    fn apply_guard_refuses_across_db_switch_and_reenables_on_return() {
        let sales = conn_identity_for("conn-a", "sales");
        let inventory = conn_identity_for("conn-a", "inventory");
        assert!(!conn_identity_matches(&sales, &inventory));
        // Switching BACK re-enables the dimmed tab — staged grid edits are
        // never dropped by a switch, only inert while away (resolved
        // deviation 11's grid half).
        assert!(conn_identity_matches(&sales, &conn_identity_for("conn-a", "sales")));
    }

    /// Admin singleton: a db switch now yields Replace (stale staged admin
    /// edits must never survive a context switch — design §5 row 4).
    #[test]
    fn admin_open_decision_replaces_across_db_switch() {
        let mut tabs = Tabs::default();
        tabs.open(ResultTab {
            id: 0, title: "Správa serveru".into(), pinned: false,
            preview_key: Some(admin_panel::ADMIN_PREVIEW_KEY.to_string()),
            conn_identity: conn_identity_for("conn-a", "sales"),
            content: TabContent::Text { text: String::new() },
        });
        assert!(matches!(
            admin_open_decision(&tabs, &conn_identity_for("conn-a", "inventory")),
            AdminOpenDecision::Replace(_)
        ));
        assert!(matches!(
            admin_open_decision(&tabs, &conn_identity_for("conn-a", "sales")),
            AdminOpenDecision::Activate(_)
        ));
    }

    /// Monitor tab singleton key widens automatically → one monitor tab
    /// per (conn, db) — consistent with its DATA_SIZE tile being
    /// per-database (design §5 row 5).
    #[test]
    fn monitor_preview_key_scopes_per_database() {
        assert_ne!(
            format!("monitor:{}", conn_identity_for("conn-a", "sales")),
            format!("monitor:{}", conn_identity_for("conn-a", "inventory")),
        );
    }

    /// CLI sentinel unchanged and never equal to any composite identity.
    #[test]
    fn cli_sentinel_is_disjoint_from_composites() {
        assert!(!conn_identity_matches(CLI_CONN_IDENTITY, &conn_identity_for("conn-a", "sales")));
        assert!(!conn_identity_matches(&conn_identity_for("cli", "x"), CLI_CONN_IDENTITY));
    }
}
```

  (If the `ResultTab`/`Tabs` fixture shape differs — match the existing `admin_open_decision` tests' construction exactly; `TabContent::Text` is the cheapest content stand-in they use.)
- [ ] **Step 3: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui identity_audit` (these should pass FIRST TRY if T3/T5 were correct — a failure here is a real regression, stop and fix it, do not adjust the test).
- [ ] **Step 4: Write the audit record** as a doc comment on `conn_identity_for` (main.rs) — the design §7 table condensed: "N stamp sites verified <date> (grep census in commit <hash>): all funnel through current_conn_identity(); 9 guard sites verified, zero weakened; intentionally guard-free surfaces re-affirmed: monitor (held connection, identity is only a tab key), backup (explicit id + existence check by design), compare (self-contained swapped configs), read-only artifact tabs (stamped, never checked)."
- [ ] **Step 5: Full run** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-mcp -p dbc-core -p dbc-state` (zero warnings).
- [ ] **Step 6: Commit** — `test(ui): identity-widening audit — full stamp/guard census, no guard weakened (sidebar T7)`.

---

### Task 8 (T8): sweep — history label, docs/release notes, v0.20.0, final gates

**Files:**
- Modify: `crates/dbc-ui/src/history_panel.rs`
- Modify: root `Cargo.toml` (version)
- Modify: `docs/superpowers/specs/drafts/sidebar-connections-design.md` (append a short "as-built deltas" footer referencing this plan's resolved deviations — the spec stays the record)
- Modify: the project memory file (`db-client-project.md` in the auto-memory dir) per phase convention

- [ ] **Step 1: History label test** (in `history_panel.rs`'s or main.rs's test mod, matching where its tests live):

```rust
    #[test]
    fn history_conn_label_appends_db_only_when_non_default() {
        assert_eq!(history_conn_label("prod", None), "prod");
        assert_eq!(history_conn_label("prod", Some("inventory")), "prod/inventory");
    }
```

- [ ] **Step 2: Implement** (design §5 row 8 — pure display string, no schema migration; dedup `sql + connection + window` naturally scopes per db):

```rust
/// Design §5 row 8: „{name}/{db}" when the active db ≠ default, plain
/// name otherwise. Display text only — history keeps recording names,
/// never URLs/credentials (design §4.6). The known name-collision
/// lossiness (rename/delete → "cli") is unchanged and out of scope.
pub(crate) fn history_conn_label(name: &str, non_default_db: Option<&str>) -> String {
    match non_default_db {
        Some(db) => format!("{name}/{db}"),
        None => name.to_string(),
    }
}
```

  and `active_connection_name_for_history` returns `history_conn_label(&c.name, self.active_database.as_deref())`.
- [ ] **Step 3: Release-note / docs sweep.** Append to the design doc an "As-built (v0.20.0)" footer listing: (a) the resolved deviations table from this plan's header (copy the 13 items); (b) the disclosed limits — 2000-database cap with its in-UI Notice, `datallowconn`/ONLINE filters, snapshot LRU cap 8; (c) the behavioral release notes — *same-connection database switch now invalidates pending Apply/script/CSV/admin writes (safety fix); dirty staged ADMIN edits prompt a confirm before a context switch; dirty GRID edits are never dropped — the tab dims and re-enables when you switch back*; (d) recorded follow-ups (NOT this phase): backup/restore of non-default databases (pg trivial, MSSQL `RESTORE … MOVE` needs its own safety pass — design §5 row 6), palette items for databases (design §5 row 14), MSSQL `USE`-based switching permanently rejected (design §3.3). Update the project memory file: phase shipped, v0.20.0, key invariants (`resolve_active` single-site rule, `\u{1F}` identity, legacy-store-key rule).
- [ ] **Step 4: Version bump** — root `Cargo.toml` `[workspace.package] version = "0.20.0"` (re-check main's current version at merge time; take the next free minor).
- [ ] **Step 5: Final gates** (verification-before-completion — run ALL, paste outputs):
  - `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` — green.
  - `%USERPROFILE%\.cargo\bin\cargo.exe build --workspace` and `%USERPROFILE%\.cargo\bin\cargo.exe build --workspace --release` — zero warnings, both profiles.
  - `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui` manual smoke: multi-root expand, db double-click switch, vault-locked expand prompts once, dirty-admin switch confirms, compare same-connection two databases, history shows `name/db`.
- [ ] **Step 6: Commit** — `chore: sidebar-connections release notes + v0.20.0 (sidebar T8)`.

---

## Open questions deliberately DECIDED (flag to the user at review, no blocker)

1. **Double-click (not single-click) switches context** — design §2.1's own decision, kept: browsing must stay side-effect-free, and single-click is selection everywhere else in the app.
2. **Admin-dirty confirm on switch** (resolved deviation 11) — the design said "existing warning", the user's risk list said "confirm"; the plan implements the confirm via the existing discard-confirm infra. If the user prefers the lighter posture, T5 Step 4's `dirty_admin_change_count` gate is one `if` to delete.
3. **File engines keep their Database row; the CLI root doesn't** (deviations 5/12) — both follow "smallest correct shape": the row exists exactly where it is a live switch target.
4. **A no-op switch (double-clicking the already-active row) does nothing** rather than re-running `test_connect` — ⟳ owns re-validation (noted inline in `switch_to_database`).
