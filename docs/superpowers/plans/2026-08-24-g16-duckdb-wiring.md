# G16 DuckDB Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers per task, a **sonnet** adversarial review per task, and a **default-model** final review once all tasks land (G13/G15 staffing convention). NO docker, NO external server anywhere in this phase: DuckDB is embedded, every "live" test is a plain `#[test]`/`#[tokio::test]` over `tempfile` paths — run the relevant live tests inside every task, not just at the end.

**Goal:** Make the existing-but-unreachable DuckDB support real: `dbc-driver-duckdb` (v0.5-era, 30+ tests, fully reviewed) exists but no `Engine::Duckdb` variant does — G16 adds the variant, wires `connect.rs`/`dbc-mcp`, decides every feature gate across the G1–G14 surface, adds DuckDB backup/restore (`COPY FROM DATABASE`) and plan-view support (`EXPLAIN (FORMAT JSON)`), and widens the read-guard/auto-limit for DuckDB's idiomatic `FROM t`/`DESCRIBE`/`SUMMARIZE` forms.

**Architecture:** Three moves (design §0): (1) **Variant** — `dbc_state::Engine::Duckdb`; the compiler's non-exhaustive-match errors across 15 files ARE the wiring checklist (no `_ =>` wildcards over `Engine`, house rule). (2) **Dialect** — NO new `dbc_core::Dialect` variant: DuckDB maps to `Dialect::Postgres` everywhere (`"…"`-doubling ident quoting, trailing `LIMIT n`, `$tag$` dollar-quote splitting are all exactly DuckDB's rules — G12 curation item 2 delivered). The only dbc-core change is guard/auto-limit widening for leading `FROM`/`DESCRIBE`/`SUMMARIZE`/`PIVOT`/`UNPIVOT` (§5). (3) **Mechanics + proof** — new backup (SQL-statement `ATTACH`/`COPY FROM DATABASE`/`DETACH` over one dedicated connection) and restore (magic-sniff + `fs::copy`), a capture-gated `EXPLAIN (FORMAT JSON)` parser, and an embedded live tier for every transactional claim. **G16 writes zero driver code** (design §0). User-facing ON-flips (the engine-picker cycle + `backup_restore_available`) land ONLY in the final task, after the whole embedded tier is green.

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no new primitive; the dialog's conditional-block idiom used for the helper row is the existing `if ui.engine == Engine::Mssql` reassignment pattern), `duckdb = "~1.10504.0"` with `bundled` (already pinned in `dbc-driver-duckdb/Cargo.toml`; joining dbc-ui/dbc-mcp's graphs makes the first clean build noticeably longer — one-time, no action), `serde_json` (already a dbc-ui dep) for the plan parser.

**Spec:** `docs/superpowers/specs/drafts/g16-duckdb-wiring-design.md` — binding. Every API claim below is grounded against branch `feature/g16-duckdb-wiring` (off main v0.18.0, post-G15): `crates/dbc-driver-duckdb/src/lib.rs`, `crates/dbc-core/src/{guards.rs,split.rs,ddl.rs,connection.rs}`, `crates/dbc-ui/src/{connect.rs,connections_ui.rs,main.rs,runner.rs,plan.rs,monitor.rs,monitor_sql.rs,admin_panel.rs,admin_sql.rs,backup.rs,csv_import.rs,sandbox.rs}`, `crates/dbc-state/src/config.rs`, `crates/dbc-mcp/src/connect.rs`. Re-locate every line reference **by symbol, not line number** if anything else merges first.

**Resolved deviations from the design doc** (each also called out inline at its task):

1. **Version:** design says 0.16.0 — stale (written before G14/G15 merged). Main is at **0.18.0**; T6 bumps to **0.19.0**.
2. **T4 ∥ T5 hint is dead:** both backup (T4) and plan view (T5) touch `main.rs` AND `runner.rs` — they serialize (single-writer rule), T4 → T5.
3. **Analyze-of-a-WRITE is REFUSED for DuckDB** (design §8 assumed `run_analyze_write`'s BEGIN→ROLLBACK works; ground truth refutes it): the DuckDB driver's `query()` clones a FRESH session off the shared root per call, independent of `execute()`'s persistent `exec_conn` (driver's own `exec_conn` doc comment; identical structural property `runner.rs::analyze_write_tests`' long doc comment documents for sqlite). A `BEGIN` issued via `execute()` is invisible to the `query()` that runs `EXPLAIN (ANALYZE, …)`, so the analyzed write would **durably commit** while the UI claims "změny vráceny zpět". T5 refuses analyze-of-write for DuckDB honestly (Czech message + belt-and-braces runner guard + a pin test of the session property). Analyze of READ statements stays ON (no transaction needed).
4. **`parse_plan(Duckdb)` never errors:** design §8's two branches are reconciled as JSON-parser-with-raw-fallback — T3 lands the pre-decided raw single-root fallback (button works from day one), T5 adds the capture-gated JSON parser; malformed JSON degrades to the raw root (verbatim text — that IS the fail-closed posture for a plan viewer), pinned by test.
5. **Payload column:** DuckDB's EXPLAIN result set is `(explain_key, explain_value)` with the payload in the SECOND column, but `drain_single_text_cell`/`dispatch_plan_query` read cell (0,0). T5 threads a `payload_col` through (capture-pinned).
6. **Backup DETACH is best-effort ALWAYS once ATTACH succeeded** (new finding, not in the design): the driver's registry root is shared process-wide and `ATTACH` is catalog-level, so a failed `COPY` that skipped `DETACH` would leak a `__dbc_backup` attachment into every other session on that file for as long as any root holder lives.
7. **Backup dialog `command_line`:** the real source db name comes from `SELECT current_database()` at RUN time (design decision); at DIALOG time it isn't known without a query, so the preview renders the file-stem-derived name (DuckDB names a file database after its stem) — display-only, execution stays engine-derived; a test pins that both agree for a normal path.
8. **Not-compile-broken sites are an explicit checklist:** `admin_entry_state` has a pre-existing `Some(_)` wildcard, and `monitor_available`/`analyze_button_visible`/`backup_restore_available`/`needs_secret`/`test_needs_vault_prompt` are `matches!`/boolean forms — the compiler will NOT flag any of them. T3 lists each with its required arm/edit and test.
9. **ON-flips (design implies wiring==on):** per the standing G15 flip discipline, T6 owns the two real user-facing switches — `next_engine` (until then no UI path can CREATE a DuckDB connection; a hand-edited `config.toml` works earlier, which is exactly the branch-internal validation surface we want) and `backup_restore_available(Duckdb)` (T3 gates it OFF explicitly, T4 builds the mechanics, T6 flips after the suite is green).

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- **Zero warnings** in plain AND test builds for every crate touched. New pub items get doc comments; no `#[allow(dead_code)]` without a named removal owner.
- GPUI pin `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no upgrade, no new primitives.
- **No `_ =>` wildcard over `Engine` may be introduced anywhere** (house rule: wildcards would let the NEXT engine skip this checklist). The one pre-existing wildcard (`admin_entry_state`'s `Some(_)`) gets an explicit `Duckdb` arm ABOVE it.
- **Write invariant (§3-novela, project-wide):** every write reaches `Connection::execute` only through a confirm modal showing the exact SQL, or a sanctioned runner-owned method with explicit transaction discipline, plus the SHARED read-only guard (`runner::guard_not_read_only`/`spec_is_read_only`). **G16 adds NO new sanctioned member** — the DuckDB backup statements ride the EXISTING G11 backup entry (`execute()`'s sanctioned-caller doc list in `dbc-core/src/connection.rs` gets that entry's text amended once, in T4 — an amendment, not a new entry).
- **Read-only is dual-enforced for DuckDB** (unlike MSSQL, like sqlite): engine-side `AccessMode::ReadOnly` (`DuckdbConnection::new_with_options(path, true)`) AND the shared client-side guards. The driver's mixed-mode policy guarantees the engine-side layer can't be silently lost to root sharing. §5's allowlist widening never weakens layer 2 — the `WRITE_KEYWORDS` blacklist still scans every token of every statement.
- **No vault involvement for DuckDB:** file-based engine — no password exists. `engine_is_file_based` (T3) keeps the master-password prompt away at every site; no secret is ever fetched, held, or embedded in SQL. The backup `ATTACH` embeds only a user-chosen file path, escaped by `''`-doubling.
- **Error hygiene:** the driver already scrubs PID/exe-path from lock errors (`translate_open_error`) and ships Czech messages (`mixed_mode_error`, the `locked` translation) — the UI surfaces driver messages **verbatim**, never rewords, never adds process-identifying data. Czech user-facing strings exactly as quoted in the tasks below.
- **Single-writer serialized files:** `main.rs`, `runner.rs`, `connections_ui.rs`, `plan.rs`, `backup.rs` are single-writer across tasks ⇒ **T3 runs SOLO**; **T4 → T5 → T6 serialize** (all three touch `runner.rs`, T4/T5 both touch `main.rs`). T1 ∥ T2 are disjoint crates and parallel.
- **Deliberate red window:** T1's variant compile-breaks `dbc-ui` and `dbc-mcp` until T3's sweep — that is the FEATURE (the checklist). T1 verifies `-p dbc-state` only; T2 verifies `-p dbc-core` only; T3 restores the full workspace to green and every task after T3 verifies the full set.
- **Live tier = embedded:** every DuckDB integration test is a plain `tempfile`-backed test. Driver fixture quirk (copy it): create the temp path but DELETE the file before letting DuckDB create the database — a pre-existing empty file is not a valid DuckDB database.
- **Versioning:** T6 bumps `[workspace.package] version` (root `Cargo.toml`, currently `0.18.0`) to `0.19.0` (next unclaimed minor — re-check main at merge time per the G15 convention).

### Task dependency graph

| Task | Name | Depends on | Files (crate-relative) | Batch |
|---|---|---|---|---|
| T1 | `Engine::Duckdb` + serde pins | — | `dbc-state/src/config.rs` | A (parallel) |
| T2 | dbc-core guard widenings (`FROM`-family read allowlist, auto-limit) | — | `dbc-core/src/guards.rs`, `dbc-core/src/split.rs` (test only) | A (parallel) |
| T3 | THE SWEEP: connect + mcp + every `Engine` site + gate postures | T1, T2 | `dbc-ui/Cargo.toml`, `dbc-mcp/Cargo.toml`, `dbc-ui/src/{connect.rs,connections_ui.rs,main.rs,runner.rs,plan.rs,monitor.rs,monitor_sql.rs,admin_panel.rs,admin_sql.rs,backup.rs,csv_import.rs}`, `dbc-mcp/src/connect.rs` | B (SOLO) |
| T4 | Backup & restore mechanics | T3 | `dbc-ui/src/{backup.rs,runner.rs,main.rs}`, `dbc-core/src/connection.rs` (doc) | C |
| T5 | Plan view: capture → JSON parser + payload plumbing | T4 | `dbc-ui/src/{plan.rs,main.rs,runner.rs}`, `dbc-ui/tests/fixtures/duckdb_explain_*.json` | C (after T4) |
| T6 | Integration tail, ON-flips, v0.19.0 | all | `dbc-ui/src/{runner.rs,connections_ui.rs,backup.rs}`, root `Cargo.toml` | last |

Suggested batches: **{T1, T2}** parallel (disjoint crates) → **{T3}** solo → **{T4}** → **{T5}** → **{T6}**.

---

### Task 1 (T1): `dbc-state` — `Engine::Duckdb` + serde pins

**Files:**
- Modify: `crates/dbc-state/src/config.rs`

**Interfaces (produced; consumed by every later task):**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine { Postgres, Mssql, Sqlite, Duckdb }   // + Duckdb — serializes as "duckdb"
```

No other `dbc-state` change: `ConnectionConfig` already carries everything DuckDB needs — `database` doubles as the file path (sqlite precedent), `read_only` exists, `timeout_secs`/`auto_limit` are engine-agnostic, no new field, no `#[serde(default)]` dance. Back-compat is purely additive for loading old configs; the one-way cost (a config containing `engine = "duckdb"` fails to load in a pre-G16 binary via toml's unknown-variant error → `AppConfig::load` returns `Err`, the `corrupt_file_is_load_error_not_default` posture) is accepted per design §1.

- [ ] **Step 1: Write the three failing tests** (design §1's REQUIRED list) in `config.rs`'s existing `mod tests`, next to `old_config_without_mssql_options_loads`:

```rust
    #[test]
    fn pre_g16_config_without_duckdb_loads_unchanged() {
        // §1 REQUIRED (a): adding the variant must not change how existing
        // postgres/mssql/sqlite configs load — purely additive.
        let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.connections[0].engine, Engine::Postgres);
        assert_eq!(config.connections[0].mssql, None);
    }

    #[test]
    fn duckdb_connection_roundtrip_save_load() {
        // §1 REQUIRED (b): a duckdb connection (database = file path,
        // read_only) survives save/load byte-exact.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut config = sample();
        config.connections[0].engine = Engine::Duckdb;
        config.connections[0].database = r"D:\data\analytics.duckdb".into();
        config.connections[0].read_only = true;
        config.connections[0].ssh = None;
        config.save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn duckdb_serde_string_form_is_pinned() {
        // §1 REQUIRED (c): the exact `engine = "duckdb"` spelling is a
        // saved-config contract — a future enum rename must not silently
        // break existing config.toml files.
        let toml_str = r#"
[[connections]]
id = "d1"
name = "analytics"
engine = "duckdb"
host = ""
database = "D:\\data\\analytics.duckdb"
user = ""
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.connections[0].engine, Engine::Duckdb);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        config.save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains(r#"engine = "duckdb""#), "raw: {raw}");
    }
```

- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state` (compile error: no `Duckdb` variant).
- [ ] **Step 3: Add the variant** (one line, `config.rs:23`).
- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state` (all new + existing tests, zero warnings). **Do NOT build dbc-ui/dbc-mcp here** — they are now intentionally compile-broken (the T3 checklist); `vault.rs`'s own tests use `Engine::Sqlite` literals only and stay green.
- [ ] **Step 5: Commit** — `feat(state): Engine::Duckdb variant + serde back-compat pins (G16 T1)`.

---

### Task 2 (T2): `dbc-core` — read-guard + auto-limit widening for DuckDB's leading-`FROM` family

**Files:**
- Modify: `crates/dbc-core/src/guards.rs`, `crates/dbc-core/src/split.rs` (one pin test only)

**Interfaces:** no signature changes — `is_read_statement`/`is_read_statement_d`/`apply_auto_limit`/`apply_auto_limit_d` keep their exact signatures; only the internal allowlist and the pg-path first-word check widen.

**Why this is a wiring-phase change, not polish (design §5, record in the doc comments):** DuckDB's idiomatic `FROM t` / `DESCRIBE t` / `SUMMARIZE t` currently fail `READ_LEADING_KEYWORDS`, which doesn't just reject them on read-only connections — on a WRITABLE connection the G12 dispatch matrix (`runner::dispatch_statement`) routes "not provably read" statements to `execute()`, so `FROM t` would run row-less and render nothing: a wrong-result bug for the engine's most idiomatic query form. Safety: the second layer is untouched — the whole-statement `WRITE_KEYWORDS` blacklist (`INSERT`/`COPY`/`ATTACH`/`INTO`/… anywhere in the statement) still rejects; no engine in this app has a write statement LEADING with any of the five new words (on pg/sqlite they're syntax errors at worst — a fail-closed guard passing a statement the server then refuses is harmless).

- [ ] **Step 1: Write the failing tests** in `guards.rs`'s `mod tests` (new `g16_duckdb_read_forms` section) and one in `split.rs`:

```rust
    // ---------- G16 §5: DuckDB's leading-FROM family ----------

    #[test]
    fn duckdb_bare_from_describe_summarize_pivot_are_reads() {
        for sql in [
            "FROM t",
            "from big_table where x > 1 order by 1",
            "DESCRIBE t",
            "SUMMARIZE t",
            "PIVOT cities ON year USING sum(population)",
        ] {
            assert!(is_read_statement(sql), "{sql} must classify as a read");
        }
    }

    /// KNOWN, SAFE limitation (pinned, resolved against the design's §5
    /// list): DuckDB's simplified UNPIVOT syntax REQUIRES an `INTO NAME …
    /// VALUE …` clause, and `INTO` sits on the WRITE_KEYWORDS blacklist
    /// (SELECT-INTO protection, layer 2) — so the statement still
    /// classifies as "not provably read" and fails CLOSED (routed to
    /// execute on a writable connection, rejected on read-only). The
    /// leading keyword stays on the allowlist (costless, and the
    /// SQL-standard `FROM t UNPIVOT (…)` form works via `FROM`); lifting
    /// the INTO collision would mean weakening the blacklist — the wrong
    /// trade.
    #[test]
    fn duckdb_unpivot_into_form_still_fails_closed_on_the_into_blacklist() {
        assert!(!is_read_statement("UNPIVOT monthly ON jan, feb INTO NAME month VALUE amount"));
        // The SQL-standard rewrite IS a read (leading FROM, no INTO):
        assert!(is_read_statement("FROM monthly UNPIVOT (amount FOR month IN (jan, feb))"));
    }

    #[test]
    fn from_leading_statement_with_a_write_keyword_anywhere_is_still_rejected() {
        // Layer 2 (the every-token WRITE_KEYWORDS scan) is untouched by the
        // allowlist widening — fail-closed posture preserved.
        assert!(!is_read_statement("FROM t SELECT * INTO backup_t"));
        assert!(!is_read_statement("FROM t, (UPDATE u SET x = 1) s"));
        assert!(!is_read_statement("FROM t; DROP TABLE t"));
    }

    #[test]
    fn auto_limit_fires_for_leading_from() {
        assert_eq!(apply_auto_limit("FROM t", 1000), ("FROM t LIMIT 1000".to_string(), true));
        assert_eq!(apply_auto_limit("from t;", 1000), ("from t LIMIT 1000;".to_string(), true));
        // unchanged skip conditions:
        assert!(!apply_auto_limit("FROM t LIMIT 5", 1000).1);
        assert!(!apply_auto_limit("FROM t OFFSET 2", 1000).1);
        // DESCRIBE/SUMMARIZE do NOT get a limit (first word is neither
        // SELECT nor FROM — under-apply, never over-apply):
        assert!(!apply_auto_limit("DESCRIBE t", 1000).1);
    }

    #[test]
    fn select_auto_limit_behavior_is_byte_identical_to_pre_g16() {
        // The SELECT path through apply_auto_limit_pg is untouched.
        assert_eq!(
            apply_auto_limit("select * from big", 1000),
            ("select * from big LIMIT 1000".to_string(), true)
        );
        assert!(!apply_auto_limit("select * from t limit 5", 1000).1);
    }
```

  In `split.rs`'s `mod tests` (pg section — G12 curation item 2's dbc-core half; the Engine→Dialect mapping half is T3's `dialect_for_engine` test):

```rust
    /// G16 (G12 curation item 2): DuckDB-bound scripts run under
    /// Dialect::Postgres — a $tag$ dollar-quoted body's interior `;` must
    /// stay opaque, exactly the pg rule (would mis-split under
    /// Dialect::Sqlite, which has no dollar quoting).
    #[test]
    fn duckdb_bound_script_splits_dollar_quotes_under_pg_dialect() {
        let sql = "CREATE MACRO half(x) AS $body$ x ; 2 $body$;\nSELECT half(4)";
        let stmts = split_sql(sql, Dialect::Postgres).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("$body$ x ; 2 $body$"));
    }
```

  (If `split_sql`'s test helper in that module has a different name — the module uses `split_bytes_one_at_a_time`/`split_sql` per the G15 tests — match the existing pg dollar-quote tests' call shape exactly.)

- [ ] **Step 2: Run to see the new tests fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core` (the guard tests fail on the unwidened allowlist; the split test may already pass — if it does, keep it as the named curation pin and note it in the commit message).
- [ ] **Step 3: Implement.** In `guards.rs`:

```rust
/// Leading keywords that may start a read-only statement. G16 §5 widened
/// the list with DuckDB's idiomatic read forms (`FROM t`, `DESCRIBE t`,
/// `SUMMARIZE t`, `PIVOT`/`UNPIVOT`): without them these statements were
/// not "provably read", so the G12 dispatch matrix routed them to
/// `execute()` — a row-less run and an empty grid, a wrong-result bug for
/// DuckDB's most idiomatic query form. Engine-safe without an engine
/// parameter: on pg/sqlite a leading FROM/DESCRIBE/... is a syntax error
/// the server refuses — a fail-closed guard passing it is harmless. The
/// every-token WRITE_KEYWORDS scan below is the unchanged second layer.
const READ_LEADING_KEYWORDS: &[&str] =
    &["SELECT", "WITH", "EXPLAIN", "SHOW", "VALUES", "PRAGMA", "FROM", "DESCRIBE", "SUMMARIZE", "PIVOT", "UNPIVOT"];
```

  Add to that doc comment the UNPIVOT caveat: the simplified `UNPIVOT … INTO NAME … VALUE …` form still fails closed on the `INTO` blacklist entry (see `duckdb_unpivot_into_form_still_fails_closed_on_the_into_blacklist` — deliberate; the SQL-standard `FROM t UNPIVOT (…)` form is the read-classified spelling).

  In `apply_auto_limit_pg` (the private pg/sqlite body), widen the first-word check — replace:

```rust
    if first_word(&items) != Some("SELECT") {
        return (sql.to_string(), false);
    }
```

  with:

```rust
    // G16 §5: `FROM t LIMIT n` is valid DuckDB and DuckDB maps to this
    // dialect path; a leading-FROM statement can't reach pg/sqlite (syntax
    // error before the limit would matter), so the widening needs no
    // engine parameter. DESCRIBE/SUMMARIZE/PIVOT stay un-limited
    // (under-apply, never over-apply).
    let fw = first_word(&items);
    if fw != Some("SELECT") && fw != Some("FROM") {
        return (sql.to_string(), false);
    }
```

  The `Dialect::Mssql` arm of `apply_auto_limit_d` is untouched (its own `first_word != Some("SELECT")` check keeps `FROM` un-limited there, correctly — leading `FROM` is not T-SQL).

- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core` (all new + ALL existing tests, zero warnings). Do NOT test dbc-ui here (compile-broken by T1 if it already landed; dbc-core's change alone cannot break dbc-ui's compile).
- [ ] **Step 5: Commit** — `feat(core): read-guard + auto-limit widened for DuckDB FROM/DESCRIBE/SUMMARIZE/PIVOT forms (G16 T2)`.

---

### Task 3 (T3): THE SWEEP — connect arm, dbc-mcp arm, and every `Engine` site's DuckDB posture (SOLO)

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml`, `crates/dbc-mcp/Cargo.toml`, `crates/dbc-ui/src/connect.rs`, `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/src/plan.rs`, `crates/dbc-ui/src/monitor.rs` (doc/test only), `crates/dbc-ui/src/monitor_sql.rs`, `crates/dbc-ui/src/admin_panel.rs`, `crates/dbc-ui/src/admin_sql.rs`, `crates/dbc-ui/src/backup.rs`, `crates/dbc-ui/src/csv_import.rs` (test only), `crates/dbc-mcp/src/connect.rs`

**Interfaces (produced; consumed by T4–T6):**

```rust
// dbc-ui/src/connect.rs
pub(crate) fn is_in_memory_duckdb_path(path: &str) -> bool;   // ":memory:" / empty → true

// dbc-ui/src/connections_ui.rs
pub(crate) fn engine_is_file_based(e: Engine) -> bool;        // Sqlite | Duckdb

// dbc-ui/src/plan.rs
pub fn parse_duckdb_raw(is_analyze: bool, raw_text: &str) -> PlanResult;  // single-root fallback
```

**Method:** after adding T1's variant, run `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui -p dbc-mcp` and fix every non-exhaustive-match error using the matrix below; then walk the **not-compile-broken checklist** (the compiler cannot find those). Grounded full site list (by symbol; count at plan time: 341 `Engine::` occurrences across 15 files, most in tests that need no change):

**A. Compile-broken sites (the compiler enumerates these) — the exact arm each gets:**

| Site (symbol) | DuckDB arm |
|---|---|
| `connect.rs::open_config` | the full arm below (`:memory:` guard + `new_with_options`) |
| `dbc-mcp/src/connect.rs::open_for_mcp` | mirror-of-sqlite arm below, read-only **unconditionally** |
| `connections_ui.rs::engine_label` | `Engine::Duckdb => "duckdb"` |
| `connections_ui.rs::next_engine` | `Engine::Duckdb => Engine::Postgres` — input-exhaustive only; **no arm PRODUCES Duckdb until T6's flip** (comment it: `// T6 flips the cycle to actually produce Duckdb (ON-flip discipline)`) |
| `main.rs::dialect_for_engine` | `dbc_state::Engine::Duckdb => Some(dbc_core::Dialect::Postgres)` — G12 curation item 2 delivered; doc comment cites it |
| `main.rs::sql_dialect` | `dbc_state::Engine::Duckdb => dbc_core::Dialect::Postgres` |
| `main.rs::run_backup_now` | interim honest refusal — replaced by T4: `dbc_state::Engine::Duckdb => { self.status = "error: záloha pro DuckDB zatím není k dispozici".to_string(); cx.notify(); }` (unreachable from the UI anyway — checklist item B4 gates `backup_restore_available(Duckdb)` OFF — but the arm must be real, honest code, never a wildcard) |
| `main.rs::backup_file_ext` | `dbc_state::Engine::Duckdb => "duckdb"` |
| `main.rs::plan_restore` | interim `Err("obnova pro DuckDB zatím není k dispozici".to_string())` (fail-closed) — replaced by T4 |
| `monitor_sql.rs::kill_sql` | `dbc_state::Engine::Duckdb => None` (protocol interrupt ≠ server-side kill; embedded engine has no sessions) |
| `runner.rs::spec_dialect` | `dbc_state::Engine::Duckdb => dbc_core::Dialect::Postgres` |
| `runner.rs::run_monitor_refresh` | join the Sqlite arm: `dbc_state::Engine::Sqlite \| dbc_state::Engine::Duckdb => { … }` (unreachable — `monitor_available` gates it — message unchanged) |
| `plan.rs::explain_sql` | `dbc_state::Engine::Duckdb => format!("EXPLAIN (FORMAT JSON) {sql}")` (final text) |
| `plan.rs::explain_analyze_sql` | `dbc_state::Engine::Duckdb => Some(format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))` (final text; the write-analyze refusal is T5's, in `run_explain`) |
| `plan.rs::parse_plan` | `dbc_state::Engine::Duckdb => Ok(parse_duckdb_raw(is_analyze, raw_text))` — T5 upgrades to the JSON parser keeping this as fallback |
| `admin_sql.rs::roles_catalog` / `privileges_catalog` / `sizes_catalog` | join Sqlite: `Engine::Sqlite \| Engine::Duckdb => Vec::new()` |
| `admin_sql.rs::create_role` / `alter_password` / `drop_role` / `add_membership` / `remove_membership` / `create_schema` / `drop_schema` | join Sqlite: `Engine::Sqlite \| Engine::Duckdb => Vec::new()` |
| `admin_sql.rs::schema_privilege` inner `match engine` | join: `Engine::Sqlite \| Engine::Duckdb => unreachable!("refused above")` (after the early-`if` widening in B) |
| `admin_panel.rs::to_statements` `priv_columns` (both the fn at ~503 and the `match self.engine` at ~1770) | `Engine::Sqlite \| Engine::Duckdb => &[]` |
| `admin_panel.rs::parse_db_sizes` | join: `Engine::Mssql \| Engine::Sqlite \| Engine::Duckdb => { … }` (defensive; never called — admin Hidden) |
| `admin_panel.rs::parse_schema_sizes` | join the `Engine::Mssql \| Engine::Sqlite` arm with `\| Engine::Duckdb` |
| `admin_panel.rs::current_db_size_label` | `Engine::Sqlite \| Engine::Duckdb => None` |

**B. NOT compile-broken (explicit checklist — each REQUIRES its edit + test):**

1. `admin_panel.rs::admin_entry_state` — has `Some(_) if read_only` / `Some(_)` wildcards (pre-existing): insert `Some(Engine::Duckdb) => AdminEntry::Hidden,` **directly after** the `Some(Engine::Sqlite) => AdminEntry::Hidden,` arm, with comment: `// G16: embedded engine — no roles/privileges/logins to administer; same posture as Sqlite.` Add to `admin_entry_state_matrix`: `assert_eq!(admin_entry_state(Some(Engine::Duckdb), false), AdminEntry::Hidden);` and `assert_eq!(admin_entry_state(Some(Engine::Duckdb), true), AdminEntry::Hidden);`
2. `monitor.rs::monitor_available` — `matches!(engine, Postgres | Mssql)` stays; Duckdb is correctly `false`. Append one doc-comment sentence: `/// Duckdb -> false (G16): embedded engine — no server sessions, locks, or DMVs to monitor; kill_sql is None (protocol interrupt is not a server-side kill). Same posture as sqlite.` Extend `monitor_available_postgres_and_mssql_not_sqlite` with `assert!(!monitor_available(dbc_state::Engine::Duckdb));` (rename the test `monitor_available_postgres_and_mssql_not_sqlite_or_duckdb`).
3. `plan.rs::analyze_button_visible` — `!matches!(engine, Sqlite)` → Duckdb `true` (intended: DuckDB HAS an analyze mode). Extend `analyze_button_visible_hides_only_for_sqlite` with `assert!(analyze_button_visible(dbc_state::Engine::Duckdb));`
4. `backup.rs::backup_restore_available` — **gate Duckdb OFF for now**: `!matches!(engine, Engine::Mssql | Engine::Duckdb)`, doc comment: `/// G16 T3: Duckdb joins the gated set until T6's flip — the DuckDB backup/restore mechanics land in T4 and the gate flips ONLY after the embedded suite is green (ON-flip discipline; same shape as the Mssql gate above).` Update `backup_restore_available_gates_mssql_only` → rename `backup_restore_available_gates_mssql_and_duckdb_pre_flip`, assert `!backup_restore_available(Engine::Duckdb)`.
5. `connections_ui.rs` vault sites — new helper + two edits:

```rust
/// G16 §2: the two file-based engines share the "database = file path, no
/// host/port/password, no vault secret" convention. ONE predicate — a
/// missed site would mean a pointless master-password prompt (or worse, a
/// skipped one) for the wrong engine. Used by `on_dropdown_item_click`'s
/// needs_secret lookup, `test_needs_vault_prompt`, and the connection
/// dialog's file-path helper row.
pub(crate) fn engine_is_file_based(e: Engine) -> bool {
    matches!(e, Engine::Sqlite | Engine::Duckdb)
}
```

   - `on_dropdown_item_click`: `.map_or(false, |c| !engine_is_file_based(c.engine));`
   - `test_needs_vault_prompt`: `password_field_empty && !engine_is_file_based(engine) && !vault_unlocked && vault_file_exists`
   - Tests: add `duckdb_never_needs_prompt` (mirror `sqlite_never_needs_prompt` with `Engine::Duckdb`); extend `invariant_unlocked_vault_never_needs_any_prompt`'s engine array to `[Engine::Postgres, Engine::Mssql, Engine::Sqlite, Engine::Duckdb]`; unit-test the predicate itself (all four engines).
6. `connections_ui.rs::render_connection_dialog_panel` — helper row under the Databáze field. Split the builder chain: end the first `let mut panel: Div = div()…` chain right after `.child(field_row("Databáze", ui.database.clone(), *cx.theme()))`, then:

```rust
    if engine_is_file_based(ui.engine) {
        panel = panel.child(
            div()
                .text_color(cx.theme().text_muted)
                .child("u SQLite/DuckDB: cesta k databázovému souboru (host/port/heslo se nepoužijí)"),
        );
    }
    panel = panel
        .child(field_row("Uživatel", ui.user.clone(), *cx.theme()))
        // …the rest of the original chain, unchanged, through the SSH checkbox…
```

   No conditional field HIDING — mirror the sqlite convention exactly (design §2: the fields render for all engines; only this hint row is conditional).
7. `main.rs::detect_editable_pk` — **no code change** (no engine arm exists since G15's flip; DuckDB is sandbox-editable by construction: `dbc_core::quote_ident` pg-style `"…"`-doubling is exactly DuckDB's identifier quoting, `sql_value_d` emission is engine-neutral, the driver populates `is_pk`). REQUIRED matrix test rows (design §4) — add next to `mssql_engine_is_editable_with_a_mapped_pk`, reusing that test's exact `t`/`h` fixture-construction lines verbatim:

```rust
    #[test]
    fn duckdb_engine_is_editable_with_a_mapped_pk_and_read_only_still_blocks() {
        // copy the TableInfo `t` + headers `h` setup lines from
        // mssql_engine_is_editable_with_a_mapped_pk (same fixtures)
        assert!(matches!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Duckdb), Some(&t), &h),
            EditableDecision::Editable(_)
        ));
        assert_eq!(
            detect_editable_pk(Some((true, dbc_state::Engine::Duckdb)), Some(&t), &h),
            EditableDecision::NotEditable
        );
    }
```

8. `main.rs::engine_from_url` — unchanged (a `.duckdb` CLI arg is an explicit non-goal, design §3; saved connections are the only DuckDB entry point). Add one doc-comment sentence saying so.
9. `admin_sql.rs::object_privilege` / `schema_privilege` early refusals — after the existing `if engine == Engine::Sqlite` refusal, add:

```rust
    if engine == Engine::Duckdb {
        return Err("DuckDB nemá serverová oprávnění".to_string());
    }
```

   (Own message rather than widening the sqlite `if` — the existing tests pin the sqlite text exactly.) Test: `object_privilege(Engine::Duckdb, …)` and `schema_privilege(Engine::Duckdb, …)` both `Err` with that exact message.
10. `csv_import.rs::is_numeric_type_name` — no code change expected (design §4); REQUIRED test with DuckDB spellings:

```rust
    #[test]
    fn duckdb_type_names_classify_numeric_vs_quoted() {
        for numeric in ["HUGEINT", "UTINYINT", "USMALLINT", "UINTEGER", "UBIGINT",
                        "BIGINT", "DOUBLE", "FLOAT", "REAL", "DECIMAL(18,3)"] {
            assert!(is_numeric_type_name(numeric), "{numeric} must classify numeric");
        }
        for quoted in ["VARCHAR", "BLOB", "DATE", "TIMESTAMP",
                       "TIMESTAMP WITH TIME ZONE", "BOOLEAN", "UUID"] {
            assert!(!is_numeric_type_name(quoted), "{quoted} must fall to the quoted path");
        }
        // "INTERVAL" contains the "int" fragment — a known, SAFE false
        // positive: sql_value's parse gate quotes any value that isn't a
        // bare numeral anyway (this classifier's own doc comment).
        assert!(is_numeric_type_name("INTERVAL"));
    }
```

**Grounding — the `connect.rs` arm (the core of the phase, design §3):**

`dbc-ui/Cargo.toml` `[dependencies]` += `dbc-driver-duckdb = { path = "../dbc-driver-duckdb" }`. Imports: `use dbc_driver_duckdb::DuckdbConnection;`.

```rust
/// G16 §3: `:memory:` (and an empty path) is refused for DuckDB BEFORE the
/// driver is ever constructed — this app opens a fresh connection per
/// dispatch and the driver's per-path registry holds only a `Weak`, so an
/// in-memory database's entire contents would be torn down the moment each
/// dispatch's last connection drops: an empty database on every single
/// query, a data-eating trap rather than a feature. Revisit only if the
/// app ever grows a held-connection mode.
pub(crate) fn is_in_memory_duckdb_path(path: &str) -> bool {
    let trimmed = path.trim();
    trimmed.is_empty() || trimmed == ":memory:"
}
```

The `open_config` arm (place after the `Engine::Sqlite` arm):

```rust
        Engine::Duckdb => {
            // File-based engine: `database` is the file path; host/port/
            // user/password and `cfg.ssh` are ignored, byte-for-byte the
            // Sqlite arm's posture (no new divergence, no new error). No
            // vault secret is ever fetched for this engine
            // (`connections_ui::engine_is_file_based` keeps the prompt
            // away at every call site).
            if is_in_memory_duckdb_path(&cfg.database) {
                return Err(QueryError::msg(
                    "in-memory DuckDB databáze není podporována — zadejte cestu k souboru",
                ));
            }
            // Dual read-only enforcement, same as sqlite: engine-side
            // AccessMode::ReadOnly here (driver-proven by its
            // read_only_connection_rejects_writes tests), plus the SHARED
            // client-side guards at the runner choke point. Registry
            // semantics the UI inherits (all driver-implemented): same
            // file+mode roots are shared and fine; opposite-mode opens
            // fail with the driver's Czech mixed-mode error; another
            // PROCESS holding the file fails with the translated `locked`
            // error (PID-scrubbed). All surfaced VERBATIM.
            let conn = DuckdbConnection::new_with_options(cfg.database.clone(), cfg.read_only);
            Ok(OpenConnection { conn: Box::new(conn), _tunnel: None })
        }
```

**Grounding — the dbc-mcp arm** (`dbc-mcp/Cargo.toml` `[dependencies]` += `dbc-driver-duckdb = { path = "../dbc-driver-duckdb" }`; imports `use dbc_driver_duckdb::DuckdbConnection;`). Duplicate the tiny `:memory:` helper (deliberate-duplication doctrine — this file's module doc already mandates near-duplication of the UI twin, cross-reference it):

```rust
/// Twin of `dbc-ui`'s `connect::is_in_memory_duckdb_path` — deliberately
/// duplicated, not shared (this file's module doc: near-duplicate of the
/// GUI connect path, a fix to one should prompt checking the twin).
fn is_in_memory_duckdb_path(path: &str) -> bool {
    let trimmed = path.trim();
    trimmed.is_empty() || trimmed == ":memory:"
}
```

```rust
        Engine::Duckdb => {
            if is_in_memory_duckdb_path(&cfg.database) {
                return Err(QueryError::msg(
                    "in-memory DuckDB databáze není podporována — zadejte cestu k souboru",
                ));
            }
            // `true` unconditionally: MCP has no write path, so it is
            // always at least as restrictive as `cfg.read_only` — same
            // posture as the Sqlite arm above. PROCESS-CONCURRENCY
            // limitation (documented in this module's doc comment, one
            // bullet added this task): the MCP server is a separate
            // process, and DuckDB allows two processes on one file only
            // when BOTH are read-only — the app holding a read-write root
            // means this open fails with the driver's translated `locked`
            // error (and vice versa); that error already names the
            // situation in human terms, no additional handling.
            let conn = DuckdbConnection::new_with_options(cfg.database.clone(), true);
            Ok(Box::new(conn))
        }
```

Also update `open_for_mcp`'s doc comment first line (`…never SSH-tunneled, never MSSQL` → `…never SSH-tunneled, never MSSQL; DuckDB always read-only`), and add the module-doc bullet quoted in the comment above.

**Grounding — `plan.rs::parse_duckdb_raw`** (design §8's pre-decided fallback branch, landing FIRST so a wired engine never has a dead "Plán" button; T5 layers the JSON parser on top). Match the field set `model_tests::node()` uses for `PlanNode`:

```rust
/// G16 T3 (design §8, fallback branch pre-landed): a single-root
/// `PlanResult` whose `raw_text` carries DuckDB's EXPLAIN output verbatim —
/// the plan tab's raw-text surface is the primary view until T5's
/// capture-gated JSON parser lands, and remains the fail-open path for
/// output that parser doesn't recognize afterwards. Never an `Err`: a
/// wired engine must not have a dead "Plán" button (design §8).
pub fn parse_duckdb_raw(is_analyze: bool, raw_text: &str) -> PlanResult {
    PlanResult {
        root: PlanNode {
            operation: "DuckDB plán".to_string(),
            target: None,
            est_cost: None,
            est_rows: None,
            actual_rows: None,
            actual_time_ms: None,
            loops: None,
            rows_removed_by_filter: None,
            buffers: None,
            extra: Vec::new(),
            children: Vec::new(),
        },
        is_analyze,
        engine: dbc_state::Engine::Duckdb,
        total_planning_time_ms: None,
        total_execution_time_ms: None,
        top_level_hints: Vec::new(),
        raw_text: raw_text.to_string(),
    }
}
```

(NOTE, resolved deviation 5: until T5, `dispatch_plan_query`'s generic branch hands this function cell (0,0) of the EXPLAIN result — for DuckDB that is the `explain_key` column, not the payload. Harmless-but-thin raw view for two tasks; T5 fixes extraction. Do not "fix" it here — `main.rs` stays minimal in T3.)

**Steps:**

- [ ] **Step 1: Add the Cargo deps**, then run `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui -p dbc-mcp` and collect the full non-exhaustive-match error list. Cross it off against table A above — **every** error must appear in the table (a site the table missed means the plan's sweep was incomplete: add it with the matrix-consistent posture and record it in the commit message).
- [ ] **Step 2: Implement all of table A + checklist B** (arms, helper fns, dialog row, doc comments). No `_ =>` wildcards anywhere.
- [ ] **Step 3: Write the T3 test suite** (these are the design §10 connect/state-adjacent REQUIRED tests):
  - `connect.rs`, new `mod duckdb_connect_tests`:

```rust
#[cfg(test)]
mod duckdb_connect_tests {
    use super::*;
    use dbc_core::CancelToken;

    fn duckdb_cfg(path: &str, read_only: bool) -> ConnectionConfig {
        ConnectionConfig {
            id: "d1".into(), name: "duck".into(), folder: vec![],
            engine: Engine::Duckdb, host: String::new(), port: None,
            database: path.into(), user: String::new(), read_only,
            timeout_secs: None, auto_limit: None, ssh: None,
            favourite: false, mssql: None,
        }
    }

    /// Driver fixture quirk (its own test suite's convention): give DuckDB
    /// a path where NO file exists yet — it must create the database
    /// itself; a pre-existing empty temp file is not a valid database.
    fn fresh_db_path(dir: &tempfile::TempDir) -> String {
        dir.path().join("t.duckdb").to_string_lossy().into_owned()
    }

    #[test]
    fn in_memory_duckdb_path_matcher() {
        assert!(is_in_memory_duckdb_path(":memory:"));
        assert!(is_in_memory_duckdb_path("  :memory:  "));
        assert!(is_in_memory_duckdb_path(""));
        assert!(is_in_memory_duckdb_path("   "));
        assert!(!is_in_memory_duckdb_path(r"D:\data\analytics.duckdb"));
    }

    #[test]
    fn duckdb_memory_path_is_refused_before_driver_construction() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match open_config(&duckdb_cfg(":memory:", false), None, rt.handle()) {
            Err(e) => e,
            Ok(_) => panic!("expected the :memory: refusal"),
        };
        assert_eq!(
            err.message,
            "in-memory DuckDB databáze není podporována — zadejte cestu k souboru"
        );
    }

    #[test]
    fn duckdb_open_config_round_trips_a_select() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        let mut opened = open_config(&duckdb_cfg(&path, false), None, rt.handle()).unwrap();
        rt.block_on(async {
            let mut stream =
                opened.conn.query("SELECT 1 AS one", CancelToken::new()).await.unwrap();
            let mut rows = 0usize;
            while let Some(item) = stream.batches.recv().await {
                rows += item.unwrap().num_rows();
            }
            assert_eq!(rows, 1);
        });
    }

    /// Proves the arm passes cfg.read_only through to the ENGINE
    /// (AccessMode::ReadOnly), not just the client-side guard.
    #[test]
    fn duckdb_read_only_config_refuses_writes_at_the_engine() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        {
            // Create the database read-write first (read-only can't create
            // a missing file), then drop it so the path's root is free.
            let mut rw = open_config(&duckdb_cfg(&path, false), None, rt.handle()).unwrap();
            rt.block_on(async {
                rw.conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
            });
        }
        let mut ro = open_config(&duckdb_cfg(&path, true), None, rt.handle()).unwrap();
        rt.block_on(async {
            let err = ro.conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await;
            assert!(err.is_err(), "AccessMode::ReadOnly must refuse the write engine-side");
        });
    }

    /// The driver's mixed-mode policy surfaces VERBATIM through
    /// open_config's arm — same path+opposite mode while any instance of
    /// the first is alive (design §3 registry semantics).
    #[test]
    fn duckdb_mixed_mode_same_path_surfaces_the_driver_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        let mut rw = open_config(&duckdb_cfg(&path, false), None, rt.handle()).unwrap();
        rt.block_on(async {
            // Bind the rw root (roots bind lazily on first use).
            rw.conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
            let handle = tokio::runtime::Handle::current();
            // Construction succeeds — the refusal fires on first use.
            let mut ro = open_config(&duckdb_cfg(&path, true), None, &handle).unwrap();
            let err = match ro.conn.query("SELECT 1", CancelToken::new()).await {
                Err(e) => e,
                Ok(_) => panic!("expected the mixed-mode refusal"),
            };
            assert_eq!(err.code.as_deref(), Some("mixed-access-mode"));
            assert!(err.message.contains("již otevřena v jiném režimu"), "got: {}", err.message);
        });
        drop(rw);
    }
}
```

  - `dbc-mcp/src/connect.rs`, in the existing `mod tests` (mirror the sqlite forced-read-only test):

```rust
    #[tokio::test]
    async fn duckdb_forced_read_only_and_memory_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.duckdb");
        {
            let mut seed = dbc_driver_duckdb::DuckdbConnection::new(&db);
            seed.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
            seed.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await.unwrap();
        } // seed's root drops here — frees the path for the read-only open
        let mut cfg = sqlite_cfg(&db, false); // reuse the fixture, flip engine:
        cfg.engine = Engine::Duckdb;
        let mut conn = open_for_mcp(&cfg, None).await.unwrap();

        let mut stream = conn.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(item) = stream.batches.recv().await {
            rows += item.unwrap().num_rows();
        }
        assert_eq!(rows, 1);

        // Write refused — read-only forced regardless of cfg.read_only=false.
        let mut saw_error = false;
        match conn.query("INSERT INTO t VALUES (2)", CancelToken::new()).await {
            Ok(mut s) => {
                while let Some(item) = s.batches.recv().await {
                    if item.is_err() { saw_error = true; }
                }
            }
            Err(_) => saw_error = true,
        }
        assert!(saw_error, "MCP DuckDB connection must refuse the write");

        cfg.database = ":memory:".into();
        assert!(open_for_mcp(&cfg, None).await.is_err());
    }
```

  - Per-engine table tests gain their Duckdb rows (each named in checklist B; plus): `main.rs::dialect_for_engine_maps_every_engine_including_mssql` → rename `…_including_mssql_and_duckdb`, assert `dialect_for_engine(Duckdb) == Some(Dialect::Postgres)` (cite G12 curation item 2 in a comment); `main.rs::sql_dialect_is_total` gains `assert_eq!(sql_dialect(Duckdb), Dialect::Postgres)`; `monitor_sql.rs::kill_sql_per_engine` gains `assert_eq!(kill_sql(dbc_state::Engine::Duckdb, 5), None);`; `main.rs::plan_restore_tests` gains a duckdb row asserting the interim `Err` (T4 replaces the assertion); `plan.rs::explain_sql_per_engine` gains `assert_eq!(explain_sql(Duckdb, "SELECT 1"), "EXPLAIN (FORMAT JSON) SELECT 1")`; `plan.rs::explain_analyze_sql…` test gains `assert_eq!(explain_analyze_sql(Duckdb, "SELECT 1"), Some("EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1".to_string()))`; new `plan.rs` unit test `parse_duckdb_raw_single_root_preserves_text` (operation `"DuckDB plán"`, no children, `raw_text` byte-equal, `engine == Duckdb`, `is_analyze` passthrough).
- [ ] **Step 4: Run everything to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-mcp -p dbc-core -p dbc-state` (zero warnings; the full workspace is green again from this task on).
- [ ] **Step 5: Commit** — `feat(ui,mcp): Engine::Duckdb wired — connect arm, dispatch sweep, gate postures per G16 matrix (G16 T3)`.

---

### Task 4 (T4): Backup & restore — `COPY FROM DATABASE` backup, sniff-and-copy restore

**Files:**
- Modify: `crates/dbc-ui/src/backup.rs`, `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/src/main.rs`, `crates/dbc-core/src/connection.rs` (doc comment only)

**Interfaces (produced; consumed by T6's flip and the dialog):**

```rust
// backup.rs
pub fn build_duckdb_backup_sql(src_db_name: &str, dest_path: &str) -> Vec<String>; // exactly 3 statements
pub const DUCKDB_MAGIC_OFFSET: usize; // 8
pub const DUCKDB_MAGIC: &[u8; 4];     // b"DUCK"
pub fn duckdb_magic_ok(bytes: &[u8]) -> bool;

// runner.rs (QueryRunner methods, mirroring the sqlite pair's signatures)
pub fn run_duckdb_backup(&self, spec: ConnectSpec, dest_path: String)
    -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>;
pub fn run_duckdb_restore(&self, db_path: String, backup_path: String, read_only: bool)
    -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>;

// main.rs
enum RestorePlan { PgTool { .. }, Mssql, Sqlite, Duckdb }   // + Duckdb
```

**Grounding — backup.rs additions** (place next to `build_vacuum_into_sql` / `SQLITE_MAGIC_HEADER`):

```rust
/// G16 §7: DuckDB's supported online single-file-copy idiom — there is no
/// `VACUUM INTO`; `ATTACH` + `COPY FROM DATABASE` + `DETACH` over ONE
/// dedicated connection is the engine-blessed equivalent (copying a live
/// DuckDB file directly risks exactly the WAL/open-writer corruption
/// `VACUUM INTO` exists to avoid on sqlite). Pure builder: `dest_path`
/// single-quote-escaped by `''`-doubling (same as `build_vacuum_into_sql`);
/// `src_db_name` (fetched at RUN time via `SELECT current_database()` —
/// DuckDB names a file database after its file stem, but asking the engine
/// beats duplicating that rule client-side) goes through
/// `dbc_core::quote_ident` (pg-style `"…"` doubling — DuckDB's identifier
/// quoting exactly). These three statements are sanctioned `execute()`
/// callers under the EXISTING G11 backup entry (amended in
/// dbc-core/src/connection.rs this task — an amendment, not a new entry).
pub fn build_duckdb_backup_sql(src_db_name: &str, dest_path: &str) -> Vec<String> {
    let escaped = dest_path.replace('\'', "''");
    let src = dbc_core::quote_ident(src_db_name);
    vec![
        format!("ATTACH '{escaped}' AS __dbc_backup"),
        format!("COPY FROM DATABASE {src} TO __dbc_backup"),
        "DETACH __dbc_backup".to_string(),
    ]
}

/// G16 §7: a DuckDB database file's main header carries the ASCII bytes
/// `DUCK` at byte offset 8 (bytes 0..8 are a block checksum). NOT trusted
/// from documentation alone — verified against a freshly created database
/// by `duckdb_magic_ok_matches_a_real_database_file` (runner.rs).
pub const DUCKDB_MAGIC_OFFSET: usize = 8;
pub const DUCKDB_MAGIC: &[u8; 4] = b"DUCK";

/// Never panics on a short slice — same posture as `sqlite_magic_header_ok`.
pub fn duckdb_magic_ok(bytes: &[u8]) -> bool {
    bytes.len() >= DUCKDB_MAGIC_OFFSET + DUCKDB_MAGIC.len()
        && &bytes[DUCKDB_MAGIC_OFFSET..DUCKDB_MAGIC_OFFSET + DUCKDB_MAGIC.len()] == DUCKDB_MAGIC
}
```

**Grounding — runner.rs inner fns** (place next to `run_sqlite_backup_inner`/`run_sqlite_restore_inner`; note `drain_single_text_cell` still has its 3-arg signature in this task — T5 adds `payload_col` and updates this call site with `0`):

```rust
/// G16 T4: `QueryRunner::run_duckdb_backup`'s async body — mirrors
/// `run_sqlite_backup_inner`: shared read-only guard first (Backup stays
/// EXEMPT per G11 curation item 2 — same predicate, no new logic), ONE
/// dedicated connection opened in the config's OWN mode (never a sneaky
/// read-write open: that would trip the driver's mixed-mode policy against
/// any concurrent read-only root, and silently escalating privileges for a
/// convenience feature is the wrong trade — a read-only instance that
/// refuses the write-mode ATTACH surfaces the engine's error verbatim, the
/// posture G11 curation item 2 blessed; pinned by
/// duckdb_backup_on_read_only_config_pins_engine_behavior). The source db
/// name is asked of the engine (`SELECT current_database()`), then the
/// three build_duckdb_backup_sql statements run in order.
///
/// DETACH is best-effort ALWAYS once ATTACH succeeded (resolved deviation
/// 6): the driver's registry root is shared process-wide and ATTACH is
/// catalog-level, so skipping DETACH after a failed COPY would leak the
/// `__dbc_backup` attachment into every other session on this file for as
/// long as any root holder lives.
async fn run_duckdb_backup_inner(
    spec: ConnectSpec,
    dest_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Backup, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    let src_db =
        drain_single_text_cell(&mut *opened.conn, "SELECT current_database()", CancelToken::new())
            .await?;
    let stmts = backup::build_duckdb_backup_sql(&src_db, &dest_path);
    opened.conn.execute(&stmts[0], CancelToken::new()).await?; // ATTACH — fail here = nothing to clean
    let copy_res = opened.conn.execute(&stmts[1], CancelToken::new()).await;
    let detach_res = opened.conn.execute(&stmts[2], CancelToken::new()).await; // ALWAYS — see doc comment
    copy_res?;
    detach_res?;
    Ok(())
}

/// G16 T4: `QueryRunner::run_duckdb_restore`'s sync body — mirrors
/// `run_sqlite_restore_inner` exactly: (1) read-only HARD-BLOCK first, no
/// exemption, before ANY I/O; (2) magic sniff (`backup::duckdb_magic_ok`,
/// `DUCK` at offset 8) — refuses without copying; (3) `fs::copy` over the
/// target. If any live root holds the target (this process or another),
/// the OS copy fails loudly and is surfaced verbatim — acceptable, the
/// per-dispatch connection model makes a lingering root the exception
/// (design §13). Typed-database-name confirm friction: unchanged, same
/// modal as every restore (caller side).
fn run_duckdb_restore_inner(db_path: &str, backup_path: &str, read_only: bool) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Restore, read_only)
        .map_err(QueryError::msg)?;
    let mut header = [0u8; 16];
    let mut f = std::fs::File::open(backup_path).map_err(|e| QueryError::msg(e.to_string()))?;
    use std::io::Read;
    let n = f.read(&mut header).map_err(|e| QueryError::msg(e.to_string()))?;
    if !backup::duckdb_magic_ok(&header[..n]) {
        return Err(QueryError::msg("soubor není DuckDB databáze"));
    }
    drop(f);
    std::fs::copy(backup_path, db_path).map_err(|e| QueryError::msg(e.to_string()))?;
    Ok(())
}
```

The two `QueryRunner` methods copy `run_sqlite_backup`/`run_sqlite_restore`'s bodies verbatim (oneshot channel; backup: `self.runtime.spawn` of the inner; restore: `spawn` + `spawn_blocking` + `unwrap_or_else(|_| Err(QueryError::msg("restore task panicked")))`), swapping the inner fn names; doc comments mirror the sqlite pair's, citing this task.

**Grounding — main.rs dispatch (replaces T3's interim refusals):**

1. `RestorePlan` gains `Duckdb`; `plan_restore`'s arm becomes `dbc_state::Engine::Duckdb => Ok(RestorePlan::Duckdb),` (no filesystem touch — the magic sniff is runner-level, same division as sqlite; update the T3 interim test to assert `Ok(RestorePlan::Duckdb)` and fold the duckdb row into `plan_restore_mssql_and_sqlite_never_touch_the_filesystem`, renaming it `plan_restore_mssql_sqlite_and_duckdb_never_touch_the_filesystem`).
2. `run_backup_now`'s Duckdb arm — mirror the `Engine::Sqlite` arm byte-for-byte except: `command_line` renders the three statements joined with `"\n"`, with the DIALOG-time source name derived from the file stem (display-only preview; the engine-derived `current_database()` stays authoritative at run time — resolved deviation 7), and the dispatch calls `run_duckdb_backup`:

```rust
            dbc_state::Engine::Duckdb => {
                // Display-only preview of the source db name: DuckDB names a
                // file database after its file stem; execution re-derives it
                // from the engine (SELECT current_database()) — pinned in
                // duckdb_backup_command_line_preview_matches_engine_name.
                let display_src = std::path::Path::new(&cfg.database)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cfg.database.clone());
                let command_line = backup::build_duckdb_backup_sql(&display_src, &dest_path).join("\n");
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Backup,
                    &cfg,
                    &dest_path,
                    command_line,
                    String::new(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let spec = ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: None };
                let rx = self.runner.run_duckdb_backup(spec, dest_path.clone());
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("backup task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Backup,
                            &connection_name,
                            &database,
                            &dest_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
```

3. `run_restore_now`'s dispatch gains a `RestorePlan::Duckdb` arm — copy the `RestorePlan::Sqlite` arm verbatim, changing only `command_line` (same `format!("copy {source_path} -> {database}")` shape) and the runner call to `self.runner.run_duckdb_restore(db_path, source_path.clone(), cfg.read_only)`.
4. `dbc-core/src/connection.rs`: amend `execute()`'s sanctioned-caller doc list — extend the EXISTING G11 backup entry's text with: `; G16: DuckDB backup's ATTACH/COPY FROM DATABASE/DETACH triple (runner::run_duckdb_backup_inner) rides this same entry`.

**Steps:**

- [ ] **Step 1: Write the failing pure tests** in `backup.rs`'s test module:

```rust
    #[test]
    fn duckdb_backup_sql_shape_and_quoting() {
        let stmts = build_duckdb_backup_sql("analytics", r"D:\zálohy\o'brien.duckdb");
        assert_eq!(stmts, vec![
            r"ATTACH 'D:\zálohy\o''brien.duckdb' AS __dbc_backup".to_string(),
            "COPY FROM DATABASE \"analytics\" TO __dbc_backup".to_string(),
            "DETACH __dbc_backup".to_string(),
        ]);
        // A hostile db name is quote_ident-escaped, never interpolated raw.
        let weird = build_duckdb_backup_sql("we\"ird", "d.duckdb");
        assert!(weird[1].contains("\"we\"\"ird\""), "got: {}", weird[1]);
    }

    #[test]
    fn duckdb_magic_ok_bounds_and_offset() {
        let mut good = vec![0u8; 16];
        good[8..12].copy_from_slice(b"DUCK");
        assert!(duckdb_magic_ok(&good));
        assert!(!duckdb_magic_ok(b"DUCK"));        // magic at offset 0 is NOT a duckdb file
        assert!(!duckdb_magic_ok(&good[..11]));    // short slice never panics
        assert!(!duckdb_magic_ok(&[]));
        assert!(!sqlite_magic_header_ok(&good));   // the two sniffs never overlap
    }
```

- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui backup::` (missing fns).
- [ ] **Step 3: Implement** `backup.rs` additions; run the pure tests to green.
- [ ] **Step 4: The COPY-FROM-DATABASE availability gate FIRST** (design §13 risk 1 → converted to a test): write and run the end-to-end round trip below BEFORE wiring the dialog. If `COPY FROM DATABASE` errors as unsupported at the vendored engine (`~1.10504.0` — not expected; the idiom predates it), STOP and surface at review: the pre-decided fallback is `EXPORT DATABASE` to a sibling DIRECTORY, a real target-picker UX change that needs a human look. In `runner.rs`, new `mod duckdb_backup_restore_tests`:

```rust
/// G16 T4: DuckDB backup/restore over real temp-file databases — the
/// embedded live tier (design §10), no docker, plain #[tokio::test].
#[cfg(test)]
mod duckdb_backup_restore_tests {
    use super::*;

    fn duckdb_cfg(path: &str, read_only: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "d1".into(), name: "duck".into(), folder: Vec::new(),
            engine: dbc_state::Engine::Duckdb, host: String::new(), port: None,
            database: path.into(), user: String::new(), read_only,
            timeout_secs: None, auto_limit: None, ssh: None,
            favourite: false, mssql: None,
        }
    }

    async fn seed_db(path: &std::path::Path) {
        let mut c = dbc_driver_duckdb::DuckdbConnection::new(path);
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", CancelToken::new())
            .await.unwrap();
        c.execute("INSERT INTO t VALUES (1, 'Příliš žluťoučký'), (2, 'kůň')", CancelToken::new())
            .await.unwrap();
    }

    /// Row count of t in the database at `path`, read through the driver.
    async fn count_t(path: &std::path::Path) -> String {
        let mut c = dbc_driver_duckdb::DuckdbConnection::new_with_options(path, true);
        let mut stream = c.query("SELECT count(*) FROM t", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        buf.cell_text(0, 0)
    }

    #[tokio::test]
    async fn duckdb_backup_end_to_end_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.duckdb");
        let dest = dir.path().join("zaloha.duckdb");
        seed_db(&src).await;
        let spec = ConnectSpec::Config { cfg: Box::new(duckdb_cfg(src.to_str().unwrap(), false)), secret: None };
        run_duckdb_backup_inner(spec, dest.to_string_lossy().into_owned(), tokio::runtime::Handle::current())
            .await
            .unwrap();
        // The DEST is a real DuckDB database carrying the seeded rows…
        assert_eq!(count_t(&dest).await, "2");
        // …and the magic the restore sniff checks for (constant verified
        // against a REAL file, per design §7 — not documentation-trusted).
        let bytes = std::fs::read(&dest).unwrap();
        assert!(backup::duckdb_magic_ok(&bytes));
        assert!(!backup::sqlite_magic_header_ok(&bytes));
    }

    /// Two backups while a browse connection holds the shared root: the
    /// second succeeds ONLY if the first's DETACH really ran (a leaked
    /// __dbc_backup attachment on the shared root would collide) — the
    /// resolved-deviation-6 regression pin.
    #[tokio::test]
    async fn duckdb_backup_twice_on_a_live_root_leaves_no_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.duckdb");
        seed_db(&src).await;
        let mut browse = dbc_driver_duckdb::DuckdbConnection::new(&src);
        {
            // Bind + hold the shared root via a drained query (execute()
            // refuses row-returning statements on some drivers).
            let mut s = browse.query("SELECT 1", CancelToken::new()).await.unwrap();
            while let Some(item) = s.batches.recv().await { item.unwrap(); }
        }
        let spec = |p: &std::path::Path| ConnectSpec::Config {
            cfg: Box::new(duckdb_cfg(p.to_str().unwrap(), false)), secret: None,
        };
        let d1 = dir.path().join("z1.duckdb");
        let d2 = dir.path().join("z2.duckdb");
        run_duckdb_backup_inner(spec(&src), d1.to_string_lossy().into_owned(), tokio::runtime::Handle::current()).await.unwrap();
        run_duckdb_backup_inner(spec(&src), d2.to_string_lossy().into_owned(), tokio::runtime::Handle::current()).await.unwrap();
        assert_eq!(count_t(&d2).await, "2");
        drop(browse);
    }

    #[tokio::test]
    async fn duckdb_backup_bad_dest_surfaces_engine_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.duckdb");
        seed_db(&src).await;
        let bad = dir.path().join("neexistuje").join("x.duckdb"); // missing parent dir
        let spec = ConnectSpec::Config { cfg: Box::new(duckdb_cfg(src.to_str().unwrap(), false)), secret: None };
        let err = run_duckdb_backup_inner(spec, bad.to_string_lossy().into_owned(), tokio::runtime::Handle::current()).await;
        assert!(err.is_err());
    }

    /// §7 read-only-backup PIN (design risk 3): Backup is EXEMPT from the
    /// read-only guard, so the guard passes and what happens next is pure
    /// ENGINE behavior — an AccessMode::ReadOnly instance asked to ATTACH a
    /// new file for write. Expected and pinned: the engine refuses and the
    /// error surfaces verbatim (G11 curation item 2 posture). IF this
    /// assertion fails because the vendored engine ALLOWS it, flip the
    /// assertion to is_ok(), verify the dest file's contents, and record
    /// the observed behavior in the commit message — either behavior is
    /// acceptable; silent divergence is not.
    #[tokio::test]
    async fn duckdb_backup_on_read_only_config_pins_engine_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.duckdb");
        let dest = dir.path().join("z.duckdb");
        seed_db(&src).await;
        let spec = ConnectSpec::Config { cfg: Box::new(duckdb_cfg(src.to_str().unwrap(), true)), secret: None };
        let result = run_duckdb_backup_inner(spec, dest.to_string_lossy().into_owned(), tokio::runtime::Handle::current()).await;
        assert!(result.is_err(), "expected the read-only instance to refuse the write-mode ATTACH; got Ok — see this test's doc comment");
    }

    #[tokio::test]
    async fn duckdb_restore_happy_path_replaces_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("backup.duckdb");
        seed_db(&src).await;
        let target = dir.path().join("live.duckdb");
        std::fs::write(&target, b"stale not-a-database content").unwrap();
        run_duckdb_restore_inner(target.to_str().unwrap(), src.to_str().unwrap(), false).unwrap();
        assert_eq!(count_t(&target).await, "2");
    }

    #[tokio::test]
    async fn duckdb_restore_refuses_wrong_magic_without_copying() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fake.duckdb");
        let mut content = backup::SQLITE_MAGIC_HEADER.to_vec(); // a SQLITE file posing as duckdb
        content.extend_from_slice(b"rest");
        std::fs::write(&src, &content).unwrap();
        let target = dir.path().join("live.duckdb");
        std::fs::write(&target, b"untouched").unwrap();
        let err = run_duckdb_restore_inner(target.to_str().unwrap(), src.to_str().unwrap(), false)
            .unwrap_err();
        assert_eq!(err.message, "soubor není DuckDB databáze");
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched"); // no copy attempted
    }

    #[tokio::test]
    async fn duckdb_restore_refuses_read_only_before_any_io() {
        // backup_path deliberately nonexistent: a read-only refusal must
        // fire BEFORE the open — the error is the guard's, not file-not-found.
        let err = run_duckdb_restore_inner(r"D:\nope\live.duckdb", r"D:\nope\missing.duckdb", true)
            .unwrap_err();
        assert!(err.message.contains("čtení"), "expected the read-only guard message, got: {}", err.message);
    }

    /// Resolved deviation 7: the dialog's file-stem preview of the source
    /// db name agrees with the engine's own current_database() for a
    /// normal path (execution uses the engine's answer regardless).
    #[tokio::test]
    async fn duckdb_backup_command_line_preview_matches_engine_name() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("analytics.duckdb");
        seed_db(&src).await;
        let mut c = dbc_driver_duckdb::DuckdbConnection::new(&src);
        let mut stream = c.query("SELECT current_database()", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await { buf.push(item.unwrap()).unwrap(); }
        assert_eq!(buf.cell_text(0, 0), "analytics");
    }
}
```

- [ ] **Step 5: Implement the runner inner fns + `QueryRunner` methods**, run the module to green: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui duckdb_backup_restore_tests` (if the availability gate in Step 4 failed on `COPY FROM DATABASE`, STOP here per that step).
- [ ] **Step 6: Wire main.rs** (RestorePlan variant, `plan_restore` arm swap, `run_backup_now`/`run_restore_now` arms replacing T3's refusals) + the `connection.rs` doc amendment; update the T3 interim `plan_restore` test.
- [ ] **Step 7: Full run** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-core` (zero warnings; note `backup_restore_available(Duckdb)` is still `false`, so no UI path reaches the new arms yet — that flip is T6's).
- [ ] **Step 8: Commit** — `feat(ui): DuckDB backup (ATTACH/COPY FROM DATABASE/DETACH) + sniff-and-copy restore, gate still OFF (G16 T4)`.

---

### Task 5 (T5): Plan view — capture first, then the JSON parser + payload plumbing + the analyze-write refusal

**Files:**
- Modify: `crates/dbc-ui/src/plan.rs`, `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/runner.rs`
- Create: `crates/dbc-ui/tests/fixtures/duckdb_explain_seq_scan.json`, `crates/dbc-ui/tests/fixtures/duckdb_explain_join.json`, `crates/dbc-ui/tests/fixtures/duckdb_explain_analyze.json`

**Interfaces (produced):**

```rust
// plan.rs
pub fn plan_payload_col(engine: dbc_state::Engine) -> usize;             // Duckdb → 1, others → 0
pub fn parse_duckdb_json(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String>;

// runner.rs
fn spec_engine(spec: &ConnectSpec) -> dbc_state::Engine;                 // sibling of spec_dialect
async fn drain_single_text_cell(conn, sql, cancel, payload_col: usize) -> Result<String, QueryError>; // + payload_col
```

**Grounding — the capture-FIRST discipline (design §8, G13 curation item 5's gate paid in milliseconds).** Step 1's test runs both EXPLAIN forms through the REAL driver and pins the result-set shape every extraction decision below depends on. Expected (pre-decided branch 1): two text columns `(explain_key, explain_value)`, one row, JSON payload in the LAST column; estimated payload is an array/object of operator nodes with `name`, `children`, `extra_info`; ANALYZE adds per-operator timing/cardinality. If the capture shows NO usable JSON at the vendored version (pre-decided branch 2): `explain_sql(Duckdb)` falls back to plain `"EXPLAIN {sql}"` and `parse_plan` keeps T3's `parse_duckdb_raw` as the primary view — the button works either way; neither branch is design-blocked. **Correct every constant below against the captured fixtures — the fixtures win, this plan's expectations lose.**

- [ ] **Step 1: The capture test** (plain `#[tokio::test]` in `plan.rs` — embedded, runs always, never `#[ignore]`):

```rust
/// G16 T5 step 1 — the fixture-capture gate (G13 curation item 5's
/// discipline, embedded so it costs milliseconds): runs both EXPLAIN forms
/// through the REAL driver and pins the result shape the extraction code
/// depends on (two text columns, JSON payload in the LAST one, parseable
/// by serde_json). The eprintln'd payloads are the source of the committed
/// tests/fixtures/duckdb_explain_*.json files — re-capture whenever the
/// vendored duckdb crate is bumped.
#[cfg(test)]
mod duckdb_capture_tests {
    use super::*;
    use dbc_core::{CancelToken, Connection};

    #[tokio::test]
    async fn capture_duckdb_explain_json_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cap.duckdb");
        let mut conn = dbc_driver_duckdb::DuckdbConnection::new(&db);
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", CancelToken::new())
            .await.unwrap();
        conn.execute(
            "INSERT INTO t SELECT range, 'r' || range FROM range(1000)",
            CancelToken::new(),
        ).await.unwrap();
        conn.execute("CREATE TABLE u(id INTEGER, t_id INTEGER)", CancelToken::new())
            .await.unwrap();

        for (label, sql) in [
            ("seq_scan", "EXPLAIN (FORMAT JSON) SELECT * FROM t WHERE name = 'r5'"),
            ("join", "EXPLAIN (FORMAT JSON) SELECT * FROM t JOIN u ON u.t_id = t.id"),
            ("analyze", "EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM t"),
        ] {
            let mut stream = conn.query(sql, CancelToken::new()).await.unwrap();
            let names: Vec<String> =
                stream.columns.fields().iter().map(|f| f.name().to_string()).collect();
            let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
            while let Some(item) = stream.batches.recv().await {
                buf.push(item.unwrap()).unwrap();
            }
            assert!(buf.row_count() >= 1, "{label}: EXPLAIN returned no rows");
            let payload_col = buf.column_count() - 1;
            assert_eq!(
                payload_col,
                plan_payload_col(dbc_state::Engine::Duckdb),
                "{label}: payload column moved — update plan_payload_col AND the fixtures (cols: {names:?})"
            );
            let payload = buf.cell_text(0, payload_col);
            let parsed: serde_json::Value = serde_json::from_str(&payload)
                .unwrap_or_else(|e| panic!("{label}: payload is not JSON ({e}): {payload}"));
            assert!(parsed.is_array() || parsed.is_object(), "{label}: unexpected JSON shape");
            eprintln!("=== duckdb_explain_{label}.json ===\n{payload}\n");
        }
    }
}
```

  Run it: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui capture_duckdb_explain_json_shapes -- --nocapture` (it fails to compile until `plan_payload_col` exists — add that fn first, Step 2 — then run, then save the three eprintln'd payloads verbatim as the fixture files).
- [ ] **Step 2: `plan_payload_col` + the runner plumbing.**
  - `plan.rs`:

```rust
/// Column index of the plan payload within an EXPLAIN result set routed
/// through the generic wrap-and-run path: DuckDB returns
/// (explain_key, explain_value) with the JSON in the SECOND column
/// (capture-pinned by capture_duckdb_explain_json_shapes); pg returns a
/// single JSON column. Sqlite/Mssql never route their payloads through
/// this helper (typed rows / query_with_session respectively) — their rows
/// exist so the mapping is total.
pub fn plan_payload_col(engine: dbc_state::Engine) -> usize {
    match engine {
        dbc_state::Engine::Duckdb => 1,
        dbc_state::Engine::Postgres | dbc_state::Engine::Mssql | dbc_state::Engine::Sqlite => 0,
    }
}
```

  - `runner.rs`: add `spec_engine` next to `spec_dialect` (same match shape, returning `dbc_state::Engine`; the `ConnectSpec::Url` arm mirrors `engine_from_url`'s two-way split with the same comment), and widen `drain_single_text_cell` with a `payload_col: usize` parameter:

```rust
async fn drain_single_text_cell(
    conn: &mut dyn Connection,
    sql: &str,
    cancel: CancelToken,
    payload_col: usize,
) -> Result<String, QueryError> {
    let mut stream = conn.query(sql, cancel).await?;
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        buf.push(item?).map_err(|e| QueryError::msg(e.to_string()))?;
    }
    if buf.row_count() == 0
        || payload_col >= buf.column_count()
        || buf.cell_is_null(0, payload_col)
    {
        return Err(QueryError::msg("EXPLAIN ANALYZE nevrátil žádný řádek"));
    }
    Ok(buf.cell_text(0, payload_col))
}
```

    Update every call site: `run_duckdb_backup_inner`'s `current_database()` call and `run_mssql_backup/restore` helpers' calls (if any route through it — grep `drain_single_text_cell(`) pass `0`; `drive_analyze_write`/`drive_analyze_write_bounded`/`run_analyze_write_inner` gain and thread a `payload_col: usize` parameter, computed ONCE in `run_analyze_write_inner` as `crate::plan::plan_payload_col(spec_engine(&spec))`; existing `analyze_write_tests` pass `0` explicitly (the mock test's dialect stays `Postgres`).
  - `main.rs::dispatch_plan_query`'s non-sqlite branch: replace the fixed `(0, 0)` reads with the payload col:

```rust
                } else {
                    let payload_col = plan::plan_payload_col(engine);
                    let raw_text = if buf.row_count() == 0 || buf.cell_is_null(0, payload_col) {
                        Err("EXPLAIN nevrátil žádný řádek".to_string())
                    } else {
                        Ok(buf.cell_text(0, payload_col))
                    };
                    raw_text.and_then(|t| plan::parse_plan(engine, is_analyze, &t))
                };
```

- [ ] **Step 3: The analyze-write refusal (resolved deviation 3 — REQUIRED, safety).**
  - `main.rs::run_explain`, the `plan::AnalyzeGate::NeedsConfirm` arm, FIRST thing inside:

```rust
            plan::AnalyzeGate::NeedsConfirm => {
                if engine == dbc_state::Engine::Duckdb {
                    // G16 T5 (resolved design gap): the DuckDB driver's
                    // query() sessions are independent clones off the shared
                    // root, invisible to execute()'s persistent exec_conn —
                    // the same structural property runner.rs's
                    // analyze_write_tests document for sqlite. So
                    // run_analyze_write's BEGIN → EXPLAIN (ANALYZE …) →
                    // ROLLBACK CANNOT actually wrap the analyzed write in a
                    // transaction: the write would durably COMMIT while the
                    // UI claims "změny vráceny zpět". Refuse honestly.
                    // Pinned by runner.rs::duckdb_query_sessions_do_not_see_execute_transactions.
                    self.status = "error: EXPLAIN ANALYZE zápisu není pro DuckDB podporováno — analyzovaný zápis nelze bezpečně vrátit".to_string();
                    cx.notify();
                    return;
                }
                // …existing AnalyzeWriteConfirm modal opening, unchanged…
            }
```

  - Belt-and-braces at the runner choke point ("each layer holds on its own"), first line of `run_analyze_write_inner` after the read-only guard:

```rust
    if spec_engine(&spec) == dbc_state::Engine::Duckdb {
        // See main.rs::run_explain's Duckdb NeedsConfirm refusal — the
        // driver session model makes the BEGIN→ROLLBACK wrapper a no-op
        // for DuckDB, so this sequence must never run against it.
        return Err(QueryError::msg(
            "EXPLAIN ANALYZE zápisu není pro DuckDB podporováno — analyzovaný zápis nelze bezpečně vrátit",
        ));
    }
```

  - Test (runner.rs, in the T4 duckdb module or a small sibling): `run_analyze_write_inner` with a duckdb spec → `Err` containing `"nelze bezpečně vrátit"`, WITHOUT the db file being created (refusal precedes `open_spec`).
  - Analyze of READS is unaffected: `AnalyzeGate::Run` routes through `dispatch_plan_query` (plain query, no transaction) — covered by Step 5's end-to-end test.
- [ ] **Step 4: `parse_duckdb_json` + fixtures.** In `plan.rs`, next to the pg parser (serde structs + ITERATIVE conversion — copy `convert_pg_tree`'s explicit frame-stack shape verbatim, never a self-calling function, per the deep-recursion rule; serde_json's own 128-deep recursion limit bounds the parse itself, same argument the pg parser relies on):

```rust
/// G16 T5 (design §8 branch 1, capture-confirmed): DuckDB's
/// `EXPLAIN (FORMAT JSON)` operator-node shape. `extra_info` is a
/// string-keyed map in the vendored version (older engines emitted a
/// single string — not this pin's problem; the fixtures rule).
/// ANALYZE adds per-operator timing (seconds) and cardinality.
/// CORRECT EVERY FIELD NAME AGAINST THE CAPTURED FIXTURES before
/// finishing this task — the fixtures win over this plan's expectations.
#[derive(Debug, Deserialize)]
struct DuckPlanJson {
    name: String,
    #[serde(default)]
    extra_info: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    children: Vec<DuckPlanJson>,
    #[serde(default)]
    operator_timing: Option<f64>,
    #[serde(default)]
    operator_cardinality: Option<f64>,
}

pub fn parse_duckdb_json(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    // The payload is an ARRAY of root nodes on estimated plans and (per
    // capture) an object or array on ANALYZE — accept both.
    let roots: Vec<DuckPlanJson> = match serde_json::from_str::<Vec<DuckPlanJson>>(raw_text) {
        Ok(v) => v,
        Err(_) => vec![serde_json::from_str::<DuckPlanJson>(raw_text)
            .map_err(|e| format!("neplatný JSON plánu: {e}"))?],
    };
    let root_json = roots.into_iter().next().ok_or_else(|| "prázdné pole v odpovědi EXPLAIN".to_string())?;
    Ok(PlanResult {
        root: convert_duckdb_tree(root_json),
        is_analyze,
        engine: dbc_state::Engine::Duckdb,
        total_planning_time_ms: None,
        total_execution_time_ms: None,
        top_level_hints: Vec::new(), // DuckDB emits no engine hints
        raw_text: raw_text.to_string(),
    })
}
```

  `convert_duckdb_tree(root: DuckPlanJson) -> PlanNode`: copy `convert_pg_tree`'s `Frame` struct + loop verbatim, with this per-node field mapping: `operation` = `name` trimmed; `target` = the first of `extra_info` keys `"Table"`, `"Relation"`, `"Function"` whose value stringifies non-empty (else `None`); `est_rows` = `extra_info["Estimated Cardinality"]` parsed as f64 (else `None`); `est_cost` = `None` (DuckDB reports no cost unit); `actual_time_ms` = `operator_timing.map(|s| s * 1000.0)`; `actual_rows` = `operator_cardinality`; `loops`/`rows_removed_by_filter`/`buffers` = `None`; `extra` = EVERY `extra_info` entry as `(key, value-stringified)` pairs in map order, including the ones already lifted into `target`/`est_rows` (design §8: "rest, never dropped" — keep all for transparency). Value stringification: `Value::String(s) => s.clone()`, everything else `v.to_string()`.
  Then flip `parse_plan`'s arm to the fallback composition:

```rust
        dbc_state::Engine::Duckdb => Ok(match parse_duckdb_json(is_analyze, raw_text) {
            Ok(parsed) => parsed,
            // Fail-closed for a plan VIEWER = never render a fabricated
            // tree: unrecognized/malformed payloads degrade to the
            // verbatim raw-text single root (T3's parse_duckdb_raw).
            Err(_) => parse_duckdb_raw(is_analyze, raw_text),
        }),
```

- [ ] **Step 5: Fixture + end-to-end tests** (plan.rs test modules):
  - `duckdb_seq_scan_fixture_parses` — `include_str!("../tests/fixtures/duckdb_explain_seq_scan.json")` → root (or a descendant) has a node whose `target == Some("t")` and non-empty `extra`; tree depth ≥ 1.
  - `duckdb_join_fixture_has_two_children` — the join node carries 2 children.
  - `duckdb_analyze_fixture_carries_timings` — some node has `actual_time_ms.is_some() && actual_rows.is_some()`; `is_analyze` true.
  - `duckdb_malformed_json_degrades_to_raw_root` — `parse_plan(Duckdb, false, "!! not json")` → `Ok`, root operation `"DuckDB plán"`, raw preserved.
  - `plan_payload_col_table` — all four engines.
  - End-to-end estimated (embedded, `#[tokio::test]`): temp db → driver `query("EXPLAIN (FORMAT JSON) SELECT * FROM t")` → ResultBuffer → `cell_text(0, plan_payload_col(Duckdb))` → `parse_plan(Duckdb, false, …)` → assert the root operation is NOT `"DuckDB plán"` (i.e. the real parser handled live output, not the fallback).
- [ ] **Step 6: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (zero warnings; every pg/mssql/sqlite plan test byte-identical — the payload-col plumbing passes `0` everywhere legacy).
- [ ] **Step 7: Commit** — `feat(ui): DuckDB plan view — captured fixtures, JSON parser with raw fallback, payload-col plumbing, analyze-write refusal (G16 T5)`.

---

### Task 6 (T6): Integration tail — transactional proofs, ON-flips, v0.19.0

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/backup.rs`, root `Cargo.toml`

**Order within the task is mandatory: the §10 proof suites FIRST, flips ONLY after they are green.**

- [ ] **Step 1: The transactional proof suite** — `runner.rs`, new `mod duckdb_runner_tests` (module doc: `//! G16 §6/§10: the embedded live tier for DuckDB's pg-style transactional semantics — the same shapes the sqlite-backed tests above have, existing because sqlite's mid-tx TOLERANCE would mask the pg-style abort divergence these pin.`). Shared helpers (duplicated per the sibling-module convention the file already uses):

```rust
    /// Driver fixture quirk: DuckDB must create the file itself.
    async fn open_duckdb_test_conn() -> (tempfile::TempPath, Box<dyn Connection>) {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let conn: Box<dyn Connection> =
            Box::new(dbc_driver_duckdb::DuckdbConnection::new(&path));
        (path, conn)
    }
```

  plus a `read_one` copy (same body as `write_transaction_tests::read_one`) and the T4 module's `duckdb_cfg` shape where a `ConnectSpec` is needed. Tests:

```rust
    /// §6: drive_write_sequence commit happy path over a real .duckdb.
    #[tokio::test]
    async fn duckdb_write_sequence_commits() {
        let (_p, mut conn) = open_duckdb_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
        let statements = vec![
            crate::admin_sql::WriteStatement::from(("INSERT INTO t VALUES (1, 'a')".to_string(), None)),
            crate::admin_sql::WriteStatement::from(("UPDATE t SET n = 'b' WHERE id = 1".to_string(), Some(1))),
        ];
        drive_write_sequence(&mut *conn, &statements, CancelToken::new(), dbc_core::Dialect::Postgres)
            .await.unwrap();
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, Some("b".to_string()));
    }

    /// §6 KEYSTONE: statement 2 fails mid-sequence → the whole tx rolls
    /// back (DuckDB aborts like pg — the driver's own
    /// mid_transaction_error_aborts_like_postgres proof, now exercised
    /// through the app's sanctioned sequence), the best-effort ROLLBACK's
    /// `let _ =` discard is safe, and the connection stays usable after.
    #[tokio::test]
    async fn duckdb_write_sequence_mid_failure_rolls_back_everything() {
        let (_p, mut conn) = open_duckdb_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        let statements = vec![
            crate::admin_sql::WriteStatement::from(("INSERT INTO t VALUES (1)".to_string(), None)),
            crate::admin_sql::WriteStatement::from(("INSERT INTO t VALUES (1)".to_string(), None)), // dup PK
        ];
        let err = drive_write_sequence(&mut *conn, &statements, CancelToken::new(), dbc_core::Dialect::Postgres).await;
        assert!(err.is_err());
        assert_eq!(read_one(&mut *conn, "SELECT count(*) FROM t").await, Some("0".to_string()));
        // Usable after the abort+rollback:
        conn.execute("INSERT INTO t VALUES (2)", CancelToken::new()).await.unwrap();
        assert_eq!(read_one(&mut *conn, "SELECT count(*) FROM t").await, Some("1".to_string()));
    }

    /// PIN behind T5's analyze-write refusal: query() clones run OUTSIDE
    /// exec_conn's open transaction (driver session model). If this test
    /// ever FAILS (the count reads "1"), the driver's session model
    /// changed and T5's refusal should be re-evaluated — the two must
    /// agree.
    #[tokio::test]
    async fn duckdb_query_sessions_do_not_see_execute_transactions() {
        let (_p, mut conn) = open_duckdb_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        conn.execute("BEGIN", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await.unwrap();
        assert_eq!(read_one(&mut *conn, "SELECT count(*) FROM t").await, Some("0".to_string()));
        conn.execute("ROLLBACK", CancelToken::new()).await.unwrap();
    }

    /// G12 curation item 4 shape, DuckDB variant: a read-only script
    /// rejects the write CLIENT-SIDE via the SHARED guard — same message,
    /// no fresh read-only logic, nothing written.
    #[tokio::test]
    async fn duckdb_read_only_script_rejects_write_via_shared_guard() {
        let (_p, mut conn) = open_duckdb_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        let err = run_script_statement(
            &mut *conn, "INSERT INTO t VALUES (1)", true, dbc_core::Dialect::Postgres, None,
            &CancelToken::new(),
        ).await.unwrap_err();
        assert_eq!(err.message, "připojení je jen pro čtení");
        assert_eq!(read_one(&mut *conn, "SELECT count(*) FROM t").await, Some("0".to_string()));
    }

    /// §5 through the dispatch matrix: the idiomatic `FROM t` runs AS A
    /// READ on DuckDB (rows drained, not a row-less execute) — the
    /// wrong-result bug T2 exists to prevent, proven end to end.
    #[tokio::test]
    async fn duckdb_leading_from_statement_runs_as_read() {
        let (_p, mut conn) = open_duckdb_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1), (2), (3)", CancelToken::new()).await.unwrap();
        let n = run_script_statement(
            &mut *conn, "FROM t", true, dbc_core::Dialect::Postgres, None, &CancelToken::new(),
        ).await.unwrap();
        assert_eq!(n, Some(3)); // drained ROW count — the read path, even on read-only
    }

    /// §4 sandbox Apply end-to-end: generate_statements under
    /// sql_dialect(Duckdb)=Postgres (pg-style "…" quoting) through the
    /// sanctioned write sequence — quoted weird column name + Czech
    /// diacritics survive byte-exact.
    #[tokio::test]
    async fn duckdb_sandbox_apply_quoted_column_and_diacritics_round_trip() {
        let (_p, mut conn) = open_duckdb_test_conn().await;
        conn.execute(r#"CREATE TABLE t(id INTEGER PRIMARY KEY, "we""ird" TEXT)"#, CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'old')", CancelToken::new()).await.unwrap();
        let sql = format!(
            "UPDATE {} SET {} = {} WHERE {} = 1",
            dbc_core::quote_qualified(None, "t"),
            dbc_core::quote_ident("we\"ird"),
            crate::sandbox::sql_value_d(Some("Příliš žluťoučký kůň"), false, dbc_core::Dialect::Postgres),
            dbc_core::quote_ident("id"),
        );
        let statements = vec![crate::admin_sql::WriteStatement::from((sql, Some(1)))];
        drive_write_sequence(&mut *conn, &statements, CancelToken::new(), dbc_core::Dialect::Postgres)
            .await.unwrap();
        assert_eq!(
            read_one(&mut *conn, r#"SELECT "we""ird" FROM t WHERE id = 1"#).await,
            Some("Příliš žluťoučký kůň".to_string())
        );
    }
```

  (If `sandbox::sql_value_d`/module paths differ from `crate::sandbox::…` visibility-wise, inline the literal `'Příliš žluťoučký kůň'` with a comment naming `sql_value_d` as the production emitter — the byte-exactness assertion is the point.)
- [ ] **Step 2: Script per-file scope + CSV import** (same module):
  - `duckdb_script_per_file_continue_rolls_back_failed_file_and_commits_next` — mirror the EXISTING sqlite `drive_script` per-file test in `script_run_tests` (locate by `TxScope::PerFile`): two temp `.sql` files, file 1 = `INSERT ok; INSERT dup-pk` (fails → file tx rolled back), `ErrorPolicy::Continue`, file 2 = one good INSERT (commits). Assert final count = 1 and the event stream carries `FileFinished` for both. Connection from `open_duckdb_test_conn`, dialect `Postgres`.
  - `duckdb_csv_import_is_all_or_nothing` — mirror the EXISTING sqlite `run_csv_import_inner` bad-batch test (locate by `CsvImportEvent::Failed` in the csv test module): duckdb `ConnectSpec` (temp path seeded with the target table via the driver first — `CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)`), a CSV whose LAST row violates the PK → assert `Failed` event and, re-opening via the driver, ZERO rows (one whole-import transaction rolled back). Reuse that test's exact `CsvImportJob` construction, changing only the spec and the duplicate-key row.
- [ ] **Step 3: Run the whole embedded tier** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui duckdb` (all duckdb-named modules across runner.rs/plan.rs/connect.rs green) and then the full `-p dbc-ui -p dbc-mcp -p dbc-core -p dbc-state -p dbc-driver-duckdb` set. **Do not proceed to Step 4 until green.**
- [ ] **Step 4: THE FLIPS** (each cites the evidence in its doc comment):
  - `connections_ui.rs::next_engine` — the user-facing switch:

```rust
fn next_engine(e: Engine) -> Engine {
    match e {
        Engine::Postgres => Engine::Mssql,
        Engine::Mssql => Engine::Sqlite,
        // G16 T6 ON-flip: Duckdb enters the picker cycle only after the
        // embedded live tier (runner.rs duckdb_runner_tests +
        // duckdb_backup_restore_tests, plan.rs duckdb fixtures/capture,
        // connect.rs duckdb_connect_tests, dbc-mcp duckdb test) went green
        // on this branch — the G15 flip discipline, embedded edition.
        Engine::Sqlite => Engine::Duckdb,
        Engine::Duckdb => Engine::Postgres,
    }
}
```

    Test: `next_engine_cycles_through_all_four` — the cycle visits all four engines exactly once and returns to start.
  - `backup.rs::backup_restore_available` — back to `!matches!(engine, Engine::Mssql)`; doc comment: `/// G16 T6 ON-flip: Duckdb un-gated after the T4 embedded suite (round trip, magic pin, read-only matrix) went green. Mssql stays gated per its own note above.` Test renamed back with `assert!(backup_restore_available(Engine::Duckdb));`
- [ ] **Step 5: Version + final sweep.** Root `Cargo.toml` `[workspace.package] version` → `0.19.0` (verify main is still 0.18.0 at merge time; take the next unclaimed minor if not). `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` (refresh `Cargo.lock`), then the FULL gate: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-buffer -p dbc-diff -p dbc-driver-sqlite -p dbc-driver-postgres -p dbc-driver-mssql -p dbc-driver-duckdb -p dbc-ui -p dbc-mcp` (pg/mssql live tiers stay behind their `--ignored` gates and are NOT required for this phase — their suites must merely compile and their non-ignored tests pass) — zero warnings everywhere.
- [ ] **Step 6: Manual smoke** (a real `.duckdb` file, e.g. created by the tests): create a DuckDB connection in the dialog (cycle shows `duckdb`, helper row visible, Test button works, no master-password prompt), browse schema tree + ER diagram, preview a table, run `FROM t` in the editor (rows + auto-LIMIT suffix), stage+Apply an edit, Explain (tree) + Analyze on a SELECT (timings) + Analyze on an UPDATE (honest refusal in status), backup + restore round trip, CSV import, chart tab, compare picker shows the connection.
- [ ] **Step 7: Commit** — `feat: G16 DuckDB wiring complete — embedded tier green, picker + backup flips, v0.19.0 (G16 T6)` with the Step 3 evidence summarized in the body. Then follow superpowers:finishing-a-development-branch.

---

## Self-review (performed at plan-authoring time)

- **Spec coverage walked §-by-§:** §1 → T1; §2 → T3 (picker/label/helper row/`engine_is_file_based`/dual read-only); §3 → T3 (open_config arm, `:memory:`, SSH-ignored, registry semantics as verbatim-surfaced driver errors, mcp arm + process-concurrency doc, sweep tables A+B); §4 matrix → every row has an owner (ON rows: grid/editor/sandbox/script/CSV/compare/ER/charts/schema-tree ride the dialect mapping + driver with tests in T3/T6; auto-limit → T2; plan → T3+T5; backup → T4+T6 flip; monitor/admin/kill gated → T3 checklist B; MCP → T3); §5 → T2; §6 → T6 Step 1; §7 → T4; §8 → T3 fallback + T5 capture/parser; §9 → no UI work (recorded in Architecture); §10 suites → mapped: dbc-state (T1), connect (T3), runner tx/script/CSV/read-only/analyze (T6+T5), backup/restore (T4), guards/split (T2), plan (T5), main.rs pure-fn rows (T3), mcp (T3); §11 invariants → Global Constraints + per-task doc comments; §12 decomposition → followed with the recorded serialization fix; §13 risks → COPY FROM DATABASE gate (T4 Step 4), EXPLAIN shape (T5 Step 1), read-only backup pin (T4), compound-type cells (explicitly out of scope, driver limitation — no task, matches design), long-lived-root restore window (T4 doc comment), history FROM-unlock note (behavioral unlock only, no code — G12 §7 precedent), build cost (no action).
- **Placeholder scan:** the two T3 interim arms (`plan_restore` Err, `run_backup_now` refusal) are real, honest, fail-closed code with named replacement task (T4) — the G15 "refusal deleted at wiring time" pattern, not TBDs. T5's serde struct carries an explicit capture-wins instruction rather than pretending field names are certain — that is the capture-first discipline, with both outcomes pre-decided. Two "copy the adjacent test's fixture lines / mirror the named existing test" instructions (T3 detect_editable_pk rows, T6 script/CSV mirrors) name their exact in-repo sources.
- **Type consistency:** `is_in_memory_duckdb_path` (both twins), `engine_is_file_based`, `parse_duckdb_raw`/`parse_duckdb_json`/`plan_payload_col`, `build_duckdb_backup_sql`/`duckdb_magic_ok`/`DUCKDB_MAGIC*`, `run_duckdb_backup(_inner)`/`run_duckdb_restore(_inner)`, `spec_engine`, `drain_single_text_cell(+payload_col)` — names and signatures match across every task that produces/consumes them; `RestorePlan::Duckdb` introduced T4 and consumed only there; T3's interim `plan_restore` test is explicitly updated by T4 Step 6.
- **Gate honesty:** no task before T6 makes DuckDB reachable from the UI picker; hand-edited configs (the validation surface) are documented; `backup_restore_available` is explicitly OFF from T3 to T6 so T4's dispatch arms are landed-but-unreachable, mirroring G15's landed-but-gated tier.
