# G9 Server monitor — design

Date: 2026-08-23
Status: draft, awaiting user review
Scope: spec row G9 (see `2026-08-22-gui-target-design.md` §2 phasing table).
Mockup approved 2026-08-22. Dashboard as a special result tab, auto-refresh
5s pausable. Postgres now (driver exists); MSSQL SQL is written and gated
but cannot be exercised until the MSSQL driver (orthogonal roadmap item)
lands. SQLite has no monitor tab.

Style: terse, decision-per-bullet, matching the G5 design-pass block in the
target-UI spec.

## 0. Constraint amendment (needs user sign-off before implementation)

- §3 of the target-UI spec currently reads "Sandbox Apply is the ONLY write
  path in the app." Kill needs a write (`pg_terminate_backend()` / `KILL
  <spid>`) that is NOT a sandbox edit. **Decision: kill is a second,
  narrowly-scoped write path — a "confirmed admin action" — routed through
  the SAME `Connection::execute()` method Apply uses, gated by a confirm
  dialog that shows the exact SQL, and blocked whenever the connection's
  `read_only` flag is set.** Recommended replacement wording: "Sandbox
  Apply (grid edits) and confirmed admin actions (server-monitor kill) are
  the app's only write paths, both exclusively through
  `Connection::execute()`; MCP (future) remains read-only." The doc comment
  on `Connection::execute` in `crates/dbc-core/src/connection.rs` ("ONLY the
  sandbox Apply flow may call it") needs the same update when this phase
  lands.
- **Why not route kill through `query()` instead (avoiding the amendment)?**
  Rejected. `SELECT pg_terminate_backend(1234)` has leading keyword `SELECT`
  and contains no token from `guards::WRITE_KEYWORDS` — `is_read_statement`
  would accept it. Sending it through `query()` on a `read_only` connection
  would silently bypass the client-side read-only guard entirely, and reuse
  the ad-hoc query pipeline (history recording, auto-limit rewriting, tab
  opening) that has nothing to do with an admin action. `execute()` is the
  correct home: it already carries "this is a write, treat it specially."
- **Non-obvious risk this design must call out prominently (see §7):**
  neither engine's read-only mechanism actually blocks kill server-side.
  Postgres's `default_transaction_read_only=on` (set by `connect::
  open_config` for `cfg.read_only`) blocks `INSERT/UPDATE/DELETE` and a
  handful of specific operations in the *current transaction*, but
  `pg_terminate_backend()` is an administrative signal, not a
  transaction-scoped write, and is NOT blocked by it. SQLite's
  `SQLITE_OPEN_READ_ONLY` is moot (no monitor tab). MSSQL's `KILL` has no
  analogous session-level read-only flag either. **The app-level
  `cfg.read_only` check (client-side, before ever sending the kill command)
  is therefore not defense-in-depth — it is the ONLY enforcement.** Must be
  checked in two places (belt-and-braces, not because either alone is
  provably insufficient, but because the whole feature's safety rests on
  this one flag): the UI disables/hides the kill affordance when
  `read_only`, and the background monitor task independently refuses to
  execute a `Kill` command when the `read_only` bool captured at
  `open_monitor` time is true.

## 1. Data model — `crates/dbc-ui/src/monitor.rs` (new, no GPUI)

Pure structs + pure parsing/aggregation functions, unit-tested without a
window (same split `sandbox.rs`/`tabs.rs` already use).

```rust
pub struct ConnectionsTile { pub active: i64, pub idle: i64, pub max: Option<i64> } // max: None = "neomezeno" (MSSQL user connections=0)
pub struct LocksTile { pub waiting: i64, pub deadlocks_since_reset: i64 } // see §6 caveat on "today"
pub struct SizeTile { pub data_bytes: i64, pub wal_or_log_bytes: i64 }
pub struct PerfTile { pub cache_hit_pct: Option<f64>, pub uptime_secs: i64, pub tps: Option<f64> } // tps: None until 2nd refresh
pub struct RunningQueryRow {
    pub pid: i64, pub user: Option<String>, pub application: Option<String>,
    pub client: Option<String>, pub state: Option<String>, pub duration_secs: f64,
    pub query: Option<String>,
}
pub struct BlockingEdge { pub waiter_pid: i64, pub blocker_pid: i64, pub wait_secs: f64, pub waiter_query: Option<String>, pub blocker_query: Option<String> }
pub struct BlockingNode { pub pid: i64, pub query: Option<String>, pub wait_secs: Option<f64>, pub children: Vec<BlockingNode>, pub cycle: bool }
pub struct TableSizeRow { pub schema: Option<String>, pub table: String, pub data_bytes: i64, pub index_bytes: i64, pub toast_bytes: i64, pub row_estimate: i64 }
pub struct MonitorSnapshot {
    pub connections: ConnectionsTile, pub locks: LocksTile, pub size: SizeTile, pub perf: PerfTile,
    pub running: Vec<RunningQueryRow>, pub blocking: Vec<BlockingNode>, pub tables: Vec<TableSizeRow>,
    pub fetched_at: std::time::Instant,
}
```

- **Parse strategy (decided):** every monitor SQL statement's SELECT list
  is written so every value the client needs is already a number or text —
  durations/uptime as `extract(epoch FROM ...)::float8` / `DATEDIFF(SECOND,
  ...)` (never a native timestamp the client would have to parse). Each
  query's `QueryStream` is drained into a throwaway `dbc_buffer::
  ResultBuffer` (same helper `runner::fetch_lookup_inner` already uses for
  a one-shot drain) and read back via `cell_text(r,c)` / `cell_is_null(r,c)`
  — i.e. the generic text-cell round-trip, not a second arrow-array-reading
  code path. Rejected alternative: downcasting `RecordBatch` columns
  directly (e.g. `Int64Array`) to skip the text round-trip — rejected
  because it would be the only arrow-column-reading code in `dbc-ui`
  outside `dbc-buffer`, for a data volume (tens to low hundreds of rows
  per tile) where the string round-trip cost is irrelevant; reusing the
  tested `cell_text`/`cell_is_null` path is worth the constant factor.
- **Numeric parsing is fail-soft, not fail-closed** (this is a read-only
  display feature, not a SQL-safety guard like `guards.rs` — a bad cell
  must degrade one field, never abort the whole snapshot or panic):
  `cell_text(r,c).parse::<i64>().unwrap_or(0)` /
  `.parse::<f64>().ok()` with the tile showing "–" for a `None`. A
  permission-denied sub-query (see §6) fails that ONE query, not the whole
  refresh — §3 has the per-query error handling.
- **TPS / deadlock-rate:** both engines only expose *cumulative* counters
  (`pg_stat_database.xact_commit+xact_rollback`,
  `sys.dm_os_performance_counters` "Transactions/sec" — misleadingly named,
  it is cumulative too). `MonitorView` (§4) keeps `prev: Option<(i64,
  Instant)>` across refreshes; `PerfTile::tps` is computed client-side as
  `(now_total - prev_total) / elapsed_secs` and is `None` on the first
  refresh after opening/reconnecting. Pure function `fn compute_rate(now:
  i64, prev: Option<(i64, Instant)>, at: Instant) -> Option<f64>`,
  unit-tested directly.
- **Blocking tree builder:** `fn build_blocking_tree(edges: &[BlockingEdge])
  -> Vec<BlockingNode>` — pure, unit-tested. Roots = blockers that don't
  themselves appear as a `waiter_pid` in `edges`. Cycle-safe: a
  wait-for cycle IS a live deadlock-in-progress (the deadlock detector
  hasn't fired yet, or the engine's detector interval hasn't elapsed) —
  the builder tracks visited pids on the current path and marks a node
  `cycle: true` (rendered "cyklus (možný deadlock)") instead of
  recursing forever.
- **Size bar fraction:** `fn bar_fraction(size: i64, max_in_set: i64) ->
  f32` (0.0..=1.0, `max_in_set == 0` -> `0.0`) — pure, feeds the per-table
  size bar's width.

## 2. SQL — Postgres (driver exists today)

All seven run sequentially over ONE dedicated connection per refresh (§3);
none can run concurrently on the same `Connection` (session-sharing caveat
already documented on `Connection::execute`/`query`). `pid <>
pg_backend_pid()` excludes the monitor's own session from the
running-queries/blocking views everywhere it appears.

```sql
-- connections tile
SELECT
  count(*) FILTER (WHERE state = 'active') AS active,
  count(*) FILTER (WHERE state = 'idle')   AS idle,
  current_setting('max_connections')::int  AS max_conn
FROM pg_stat_activity WHERE datname = current_database();

-- locks & deadlocks tile
SELECT
  (SELECT count(*) FROM pg_locks WHERE NOT granted) AS waiting,
  (SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()) AS deadlocks_since_reset;

-- size tile (data vs WAL)
SELECT
  pg_database_size(current_database()) AS data_bytes,
  (SELECT coalesce(sum(size), 0) FROM pg_ls_waldir()) AS wal_bytes;

-- perf tile (cache hit, uptime, xact totals for client-side TPS delta)
SELECT
  round(100.0 * sum(blks_hit) / NULLIF(sum(blks_hit) + sum(blks_read), 0), 2) AS cache_hit_pct,
  extract(epoch FROM now() - pg_postmaster_start_time())::bigint AS uptime_secs,
  sum(xact_commit + xact_rollback) AS xact_total
FROM pg_stat_database;

-- running queries
SELECT pid, usename AS "user", application_name AS application, client_addr::text AS client,
       state, extract(epoch FROM now() - query_start)::float8 AS duration_secs, query
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid()
ORDER BY duration_secs DESC NULLS LAST
LIMIT 200;

-- blocking chains (standard "who blocks whom" pattern)
SELECT blocked_activity.pid AS waiter_pid, blocking_activity.pid AS blocker_pid,
       extract(epoch FROM now() - blocked_activity.query_start)::float8 AS wait_secs,
       blocked_activity.query AS waiter_query, blocking_activity.query AS blocker_query
FROM pg_locks blocked
JOIN pg_stat_activity blocked_activity ON blocked_activity.pid = blocked.pid
JOIN pg_locks blocking
  ON blocking.locktype IS NOT DISTINCT FROM blocked.locktype
 AND blocking.database  IS NOT DISTINCT FROM blocked.database
 AND blocking.relation  IS NOT DISTINCT FROM blocked.relation
 AND blocking.page      IS NOT DISTINCT FROM blocked.page
 AND blocking.tuple     IS NOT DISTINCT FROM blocked.tuple
 AND blocking.transactionid IS NOT DISTINCT FROM blocked.transactionid
 AND blocking.classid   IS NOT DISTINCT FROM blocked.classid
 AND blocking.objid     IS NOT DISTINCT FROM blocked.objid
 AND blocking.objsubid  IS NOT DISTINCT FROM blocked.objsubid
 AND blocking.pid <> blocked.pid
JOIN pg_stat_activity blocking_activity ON blocking_activity.pid = blocking.pid
WHERE NOT blocked.granted AND blocking.granted;

-- per-table sizes
SELECT n.nspname AS schema, c.relname AS "table",
       pg_relation_size(c.oid) AS data_bytes,
       pg_indexes_size(c.oid) AS index_bytes,
       CASE WHEN c.reltoastrelid <> 0 THEN pg_relation_size(c.reltoastrelid) ELSE 0 END AS toast_bytes,
       c.reltuples::bigint AS row_estimate
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
ORDER BY pg_relation_size(c.oid) + pg_indexes_size(c.oid) DESC;

-- kill (Connection::execute, NEVER query — see §0)
SELECT pg_terminate_backend(<validated i64 pid>)
```

**Permission caveats (flag in the UI as a per-tile "n/a", not a crash):**
`pg_stat_activity` hides `query`/`client_addr`/etc. for sessions not owned
by the connecting role unless it has `pg_monitor` (or is superuser) — PG10+
still shows the row (state, pid) but nulls the sensitive columns, it does
not error. `pg_ls_waldir()` requires `pg_monitor`/superuser and DOES raise
a permission-denied error for a plain role — the size tile's WAL half must
degrade to "n/a" independently of the data-size half succeeding.

## 3. SQL — MSSQL (written now, blocked on the driver — see §6/§8)

No `dbc-driver-mssql` crate exists yet (`connect::open_config` hard-errors
`Engine::Mssql` today: "MSSQL driver zatím není k dispozici" —
`crates/dbc-ui/src/connect.rs:95-99`). These queries are the target shape
for when that driver lands; they are NOT run against a live server by this
phase's tests (see T3/T7 in §5).

```sql
-- connections tile
SELECT
  SUM(CASE WHEN status = 'running'  THEN 1 ELSE 0 END) AS active,
  SUM(CASE WHEN status = 'sleeping' THEN 1 ELSE 0 END) AS idle
FROM sys.dm_exec_sessions WHERE is_user_process = 1;
-- max: value_in_use = 0 means "unlimited/dynamic" (UI: "max" = None)
SELECT value_in_use AS max_conn FROM sys.configurations WHERE name = 'user connections';

-- locks & deadlocks tile
SELECT COUNT(*) AS waiting FROM sys.dm_tran_locks WHERE request_status = 'WAIT';
-- misleading name: despite "/sec" this is a CUMULATIVE counter, same caveat as pg's deadlocks
SELECT cntr_value AS deadlocks_since_reset FROM sys.dm_os_performance_counters
WHERE counter_name = 'Number of Deadlocks/sec' AND instance_name = '_Total';

-- size tile (data vs log = MSSQL's WAL equivalent) — the "sp_spaceused equivalent"
SELECT
  SUM(CASE WHEN type_desc = 'ROWS' THEN size ELSE 0 END) * 8 * 1024 AS data_bytes,
  SUM(CASE WHEN type_desc = 'LOG'  THEN size ELSE 0 END) * 8 * 1024 AS log_bytes
FROM sys.database_files;

-- perf tile
SELECT (a.cntr_value * 1.0 / NULLIF(b.cntr_value, 0)) * 100 AS cache_hit_pct
FROM sys.dm_os_performance_counters a, sys.dm_os_performance_counters b
WHERE a.counter_name = 'Buffer cache hit ratio' AND b.counter_name = 'Buffer cache hit ratio base';
SELECT DATEDIFF(SECOND, sqlserver_start_time, GETDATE()) AS uptime_secs FROM sys.dm_os_sys_info;
-- cumulative; client-side delta same as pg's xact_total
SELECT cntr_value AS xact_total FROM sys.dm_os_performance_counters
WHERE counter_name = 'Transactions/sec' AND instance_name = '_Total';

-- running queries (TOP 200 mirrors the pg LIMIT — see §8 perf caveat)
SELECT TOP 200 r.session_id AS pid, s.login_name AS [user], s.program_name AS application,
       s.host_name AS client, r.status AS state,
       DATEDIFF(SECOND, r.start_time, GETDATE()) AS duration_secs, t.text AS query
FROM sys.dm_exec_requests r
JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t
WHERE r.session_id <> @@SPID
ORDER BY duration_secs DESC;

-- blocking chains
SELECT r.session_id AS waiter_pid, r.blocking_session_id AS blocker_pid,
       r.wait_time / 1000.0 AS wait_secs, tw.text AS waiter_query, tb.text AS blocker_query
FROM sys.dm_exec_requests r
CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) tw
OUTER APPLY (SELECT sql_handle FROM sys.dm_exec_requests br WHERE br.session_id = r.blocking_session_id) bx
OUTER APPLY sys.dm_exec_sql_text(bx.sql_handle) tb
WHERE r.blocking_session_id <> 0;
-- caveat: blocker_query is NULL when the blocker is idle-in-transaction (no
-- active request row in dm_exec_requests) — inherent to the DMV, not a bug.

-- per-table sizes (set-based sp_spaceused equivalent, no per-table EXEC loop)
SELECT OBJECT_SCHEMA_NAME(ps.object_id) AS [schema], OBJECT_NAME(ps.object_id) AS [table],
       SUM(CASE WHEN ps.index_id IN (0,1) THEN ps.in_row_data_page_count ELSE 0 END) * 8 * 1024 AS data_bytes,
       SUM(CASE WHEN ps.index_id > 1 THEN ps.used_page_count ELSE 0 END) * 8 * 1024 AS index_bytes,
       SUM(ps.lob_used_page_count + ps.row_overflow_used_page_count) * 8 * 1024 AS toast_bytes,
       MAX(CASE WHEN ps.index_id IN (0,1) THEN ps.row_count ELSE 0 END) AS row_estimate
FROM sys.dm_db_partition_stats ps
JOIN sys.tables t ON t.object_id = ps.object_id
GROUP BY ps.object_id
ORDER BY data_bytes + index_bytes DESC;

-- kill (Connection::execute)
KILL <validated i64 spid>
```

**Permission caveat:** every `sys.dm_exec_*`/`sys.dm_os_*`/`sys.dm_tran_*`
view requires `VIEW SERVER STATE` (SQL2022+: `VIEW SERVER PERFORMANCE
STATE` covers the perf-counter ones) — without it the DMV returns zero rows
for other sessions (not an error) or, for some counters, a permission-denied
error; same per-tile "n/a" degrade as Postgres.

## 4. Refresh loop lifecycle

- **New `QueryRunner::open_monitor`** (`runner.rs`):
  ```rust
  fn open_monitor(&self, spec: ConnectSpec, read_only: bool)
      -> (tokio::sync::mpsc::Sender<MonitorCmd>, tokio::sync::mpsc::Receiver<MonitorEvent>)
  enum MonitorCmd { Refresh { generation: u64 }, Kill { generation: u64, pid: i64 } }
  enum MonitorEvent {
      Data { generation: u64, snapshot: MonitorSnapshot },
      Error { generation: u64, message: String },
      KillResult { generation: u64, pid: i64, result: Result<u64, QueryError> },
  }
  ```
  Spawned on `QueryRunner`'s own runtime (same `self.runtime.spawn` used by
  `connect_and_run`): opens ONE `Box<dyn Connection>` via the existing
  `open_spec` helper and holds it for the tab's lifetime — no reconnect per
  tick (pg_stat_activity et al. reflect *other* sessions' activity; nothing
  requires a fresh session each poll, and reconnecting every 5s would be
  wasteful and would itself show up as a phantom connection in the
  connections tile). `read_only` is captured once at open time — the kill
  gate (§0).
- **No explicit `Close` command needed.** `MonitorCmd`'s `Sender` lives on
  `MonitorView`; when the tab closes, `Tabs::close` drops the `ResultTab` ->
  drops `TabContent::Monitor` -> drops `MonitorView` -> drops the `Sender`.
  The background task's `cmd_rx.recv()` returns `None`, the loop exits, and
  `Box<dyn Connection>` drops (closing the DB session) — same "drop tears
  everything down" pattern `OpenConnection`/`Tunnel` already rely on.
- **Cancellation of an in-flight refresh:** each `Refresh` is raced against
  the NEXT `cmd_rx.recv()` via `tokio::select!` — exactly the pattern
  `connect_and_run`'s timeout watchdog already uses (there: race against a
  timer; here: race against channel activity). If `cmd_rx.recv()` resolves
  to `None` (tab closed) while a refresh is running, the in-flight
  `CancelToken` (fresh per refresh) is cancelled and the loop exits without
  sending a stale `Data`/`Error`.
- **Overlap prevention:** `MonitorView` holds `awaiting: bool` and
  `refresh_generation: u64`. The UI-side timer (below) only sends a new
  `Refresh` when `awaiting == false`; a slow refresh (e.g. a busy server)
  simply causes that tick to be skipped rather than queued — the NEXT timer
  tick tries again. `generation` on every `MonitorEvent` is compared
  against `MonitorView::refresh_generation`; a `Data`/`Error` whose
  generation doesn't match the latest dispatched `Refresh` is dropped
  (last-dispatched-wins — same convention as `AppView::switch_generation`/
  `schema_fetch_generation`). This also naturally discards a refresh
  response that arrives after the connection was already superseded (e.g.
  user reopened the monitor tab, which is `close_by_preview_key`-style
  replace, not update-in-place — see §5).
- **Backoff on error:** `MonitorView::interval_secs` starts at 5; on
  `MonitorEvent::Error`, doubles (5→10→20→40→60, capped at 60) for the
  NEXT tick; any `MonitorEvent::Data` resets it to 5. Status line under the
  tiles shows "aktualizace selhala ({msg}) · další pokus za {n}s" while
  backed off.
- **Timer + pause + hidden-tab gating (UI thread, `cx.spawn`):** one loop
  per open monitor tab, using the SAME `cx.background_executor().timer(...)`
  primitive `grid.rs`'s export chunking already uses (`crates/dbc-ui/src/
  grid.rs:1437-1439`) — this repo's established GPUI timer idiom, as
  opposed to a raw `tokio::time::sleep` on the UI thread.
  ```rust
  cx.spawn(async move |this, cx| loop {
      cx.background_executor().timer(Duration::from_secs(interval)).await;
      let should_tick = this.update(cx, |view, _| {
          view.monitor_tab_visible() && !view.monitor.paused && !view.monitor.awaiting
      }).unwrap_or(false);
      if !should_tick { continue; } // still checked every `interval`; no catch-up burst on resume
      // dispatch MonitorCmd::Refresh{generation}, set awaiting = true
  }).detach();
  ```
  `monitor_tab_visible()`: `self.tabs.active().is_some_and(|t| t.id ==
  monitor_tab_id)` — "pause on tab hidden" is thus automatic and separate
  from the manual pause toggle (⏸/▶ in the tile row): switching away from
  the monitor tab stops ticks; switching back resumes on the next natural
  tick (no forced immediate refresh, keeping the "at most one in flight,
  at most every N seconds" invariant simple). The manual toggle sets
  `monitor.paused` and is independent — either condition alone suppresses
  a tick. The loop itself is NOT torn down by pause (cheap to keep
  sleeping); it IS torn down implicitly when `MonitorView` (and this
  `cx.spawn` future's `this: WeakEntity`) is dropped on tab close —
  `this.update` starts returning `None`/erroring and `.unwrap_or(false)`
  makes the loop a no-op forever rather than panicking; acceptable since
  the loop is dropped shortly after anyway once GPUI notices the entity is
  gone. (If review flags the "forever no-op task" as wasteful, the
  cheap fix is checking `this.upgrade().is_none()` and `break`ing —
  flagged as a nice-to-have in §8, not a correctness issue.)

## 5. UI layout (mapping onto the approved mockup)

- **Special result tab:** new `TabContent::Monitor { view: Entity<MonitorView> }`
  in `tabs.rs`, alongside `Grid`/`Text`. Opened via a top-bar action /
  palette entry "Monitor serveru" (only enabled per §6 gating), reusing the
  `preview_key`-style replace-not-stack convention with key
  `"monitor:{connection_id}"` — reopening while already open just
  activates the existing tab (one monitor tab per connection at a time).
- **Tile row (top):** four fixed-width cards, same visual language as the
  rest of the app (`rgb(0x1e1e2e)` card bg, `rgb(0x45475a)` border — the
  tokens already used throughout `connections_ui.rs`'s modal/dropdown
  panels):
  1. Connections: `{active} aktivní · {idle} idle / max {max|"neomezeno"}`.
  2. Locks & deadlocks: `{waiting} čeká na zámek` + `{deadlocks_since_reset}
     deadlocků` (label: "od posledního resetu statistik", not "dnes" — see
     §6 caveat).
  3. DB size: two-segment bar, data vs WAL/log, byte counts as text.
  4. Cache/uptime/TPS: `{cache_hit_pct|"–"}% cache hit · uptime {uptime}` ·
     `TPS {tps|"–"}`.
  Top-right of the tile row: pause/resume toggle, "poslední aktualizace
  HH:MM:SS", manual refresh button (dispatches an out-of-cycle `Refresh`
  regardless of `paused`/backoff state, still gated by `awaiting`).
- **Running queries section:** `uniform_list` (same virtualized-list
  primitive `grid.rs` uses for result rows), sorted by `duration_secs`
  desc. Columns: pid, user, application, client, state, duration (colour
  tier below), query (truncated, click/Enter opens the SAME read-only
  cell-detail popup `row_view.rs` already provides for non-editable
  cells), kill button (icon, disabled + tooltipped "spojení je pouze pro
  čtení" when `read_only`).
  - **Duration colour tiers** (reuse the exact hex families `grid.rs`
    already uses for diff tints, applied here as TEXT colour, not row bg,
    so it composes with the sort-order highlighting the grid already has):
    `< 1s` default text `rgb(0xcdd6f4)`; `1s..10s` `rgb(0xf9e2af)`
    (the `STAGED_CELL_BG` yellow family); `>= 10s` `rgb(0xf38ba8)`
    (Catppuccin red, same family as `DELETED_ROW_BG`). Thresholds are
    hardcoded constants (`DURATION_WARN_SECS = 1.0`, `DURATION_CRIT_SECS =
    10.0`) in `monitor_view.rs` — not user-configurable in this phase.
- **Blocking chains section:** indented tree from `build_blocking_tree`
  (§1) — blocker as parent, waiters as children, recursively (a waiter
  that is itself blocking someone else nests further); each node shows
  `pid · wait {wait_secs}s · {query truncated}`; a `cycle: true` node
  renders with the red duration-tier colour and the literal suffix
  "(cyklus — možný deadlock)". Clicking a pid opens the same read-only
  query-detail popup as the running-queries section (reads `waiter_query`/
  `blocker_query` off the node — no extra round-trip).
- **Per-table sizes section:** flat list (not virtualized — table count is
  bounded by the schema, unlike row count), one row per table: `schema.
  table | data | indexes | toast | ~{row_estimate} řádků | [horizontal
  bar]`. Bar width = `bar_fraction(data+index+toast, max_in_set)` (§1) as a
  plain `div().w(px(...))` fill — no charting dependency needed.
- **Read-only mode:** the whole tab is otherwise pure display (no sandbox
  edit affordances apply — it isn't a preview tab) except the kill button,
  which is the one interactive/write element; §0/§6 cover its gating.

## 6. Kill flow

- **Confirm dialog:** reuses the existing single-modal-at-a-time
  infrastructure — new `connections_ui::ModalState::KillConfirm { pid: i64,
  label: String, sql: String, tab_id: u64 }` variant rather than a
  monitor-local modal, because `run_query_with` already refuses to run
  while `self.modal.is_some()` and the dropdown/palette already refuse to
  open a second modal over an existing one; a kill confirmation is exactly
  the kind of blocking dialog that invariant exists for. Rendered the same
  way `render_modal_overlay` renders `ConnectionDialog` (centered,
  `.occlude()`, backdrop). Content: "Opravdu ukončit proces {pid} ({user} ·
  {application} · běží {duration})?" + the literal SQL that will run (`SELECT
  pg_terminate_backend(1234)` / `KILL 1234`) in a monospace block — same
  "show the exact generated SQL" principle as the Apply dialog (G5 design
  pass, §1 of the target-UI spec). Buttons: "Zrušit" / "Ukončit proces".
- **Gating (belt-and-braces per §0):** the kill icon itself is
  `disabled`+tooltipped when `MonitorView::read_only` is true (client-side,
  first line); confirming still dispatches `MonitorCmd::Kill`, and the
  background task independently checks its OWN captured `read_only` before
  calling `conn.execute()`, returning `MonitorEvent::Error` with a Czech
  message ("spojení je pouze pro čtení — zabití procesu odmítnuto") if
  somehow reached with it true. Belt-and-braces here is not theater (per
  §0's risk note) — it's two independent code paths implementing the ONLY
  real enforcement, so a bug in one doesn't remove the gate entirely.
- **pid validation:** `pid`/`spid` values displayed and sent for kill
  originate ONLY from a just-fetched `RunningQueryRow`/`BlockingNode` (an
  `i64` already parsed out of the driver's own typed result, never
  user-typed text) — `sql_value`-style quoting/escaping (as sandbox.rs uses
  for user-entered cell values) is unnecessary and NOT used; the value is
  formatted as a bare integer directly. Defensive `debug_assert!(pid > 0)`
  before formatting, matching `sandbox.rs`'s "must never be constructed
  wrong" precedent for `generate_statements`' PK WHERE.
- **On confirm → execute → result:** `Connection::execute(sql, cancel)`
  returns `Result<u64, QueryError>` (Apply's existing write-path
  contract). Success (regardless of the returned affected-rows count —
  `pg_terminate_backend`/`KILL` don't have a meaningful "1 row" contract
  the way Apply's UPDATE/DELETE do): close the dialog, status bar
  "proces {pid} ukončen", and dispatch an immediate out-of-cycle
  `MonitorCmd::Refresh` (bypassing the timer) so the running-queries list
  reflects it without waiting up to 5s. Failure: dialog stays open with
  the error text below the SQL block (same "error stays in the modal,
  edits/state aren't lost" precedent as Apply's rollback-error UX) —
  common failure: the target pid already exited between snapshot and
  click (`ERROR: signal 15 (SIGTERM) sent`? no — Postgres returns `false`
  as the function's row value for "pid not found", not an error, this
  surfaces as `Ok(_)` — the dialog message should note the process may
  simply have already ended if a follow-up refresh shows it gone).

## 7. Engine gating

- `fn monitor_available(engine: dbc_state::Engine) -> bool { engine ==
  dbc_state::Engine::Postgres }` — pure, unit-tested, mirrors
  `detect_editable_pk`'s existing MSSQL-exclusion precedent
  (`main.rs:172`). `Sqlite` -> `false` (spec: no monitor tab at all).
  `Mssql` -> `false` FOR NOW (SQL is designed per §3, but `connect::
  open_config` cannot open an MSSQL connection at all today — gating
  follows the driver, not the spec's intent, so this flips to `true`
  automatically once `dbc-driver-mssql` lands and `open_config`'s
  `Engine::Mssql` arm stops erroring; no monitor-side code change needed
  beyond this one function).
  - Alternative rejected: only compute `monitor_available` on the config
    path and always show it for MSSQL configs anyway "since the SQL is
    ready" — rejected, because clicking it would just surface the
    existing "MSSQL driver zatím není k dispozici" connect error, which is
    confusing for a menu item that looks like a monitor feature is
    broken rather than absent.
- Menu/palette entry: enabled only when `monitor_available(engine)` for
  the ACTIVE connection — `cfg.engine` for a saved connection,
  `engine_from_url(url)` (existing helper, `main.rs:126-132`) for the
  CLI-arg back-compat path, `None`/disabled when there's no active
  connection at all. No connection selected → entry hidden entirely
  (same convention `TreeEvent`/preview actions already use when there's
  nothing to act on).

## 8. Task decomposition

| # | Task | Files | Depends on | Tests |
|---|---|---|---|---|
| T1 | Data model + pure parsers (`MonitorSnapshot` et al., `compute_rate`, `build_blocking_tree`, `bar_fraction`) | `crates/dbc-ui/src/monitor.rs` (new) | — | Unit only: rate-delta edge cases (first refresh `None`, zero-elapsed guard), cycle-safe tree building (linear chain, single cycle, self-referential defensive case), bar fraction (`max=0`), fail-soft numeric parse on garbage text |
| T2 | Postgres SQL constants (§2) | `crates/dbc-ui/src/monitor.rs` (submodule `sql::pg`) | — | Unit: every non-kill query passes `dbc_core::is_read_statement` (regression guard — a future edit that turns one into a write must fail CI, not just review) |
| T3 | MSSQL SQL constants (§3) | `crates/dbc-ui/src/monitor.rs` (submodule `sql::mssql`) | — | Unit: leading-keyword smoke check only (`is_read_statement`'s keyword set is pg/sqlite-flavoured, not authoritative for T-SQL — documented as N/A for MSSQL text); no live-server test possible (no driver) |
| T4 | `QueryRunner::open_monitor` — background task, channel plumbing, sequential 7-query refresh via T1 parsers, `Kill` via `execute()` + read-only gate | `crates/dbc-ui/src/runner.rs` | T1, T2 | Unit-testable only for the pure pieces already covered by T1; the task-loop plumbing itself needs T7's docker test |
| T5 | `MonitorView` GPUI entity: `TabContent::Monitor`, open action + `monitor_available` gating, the `cx.spawn` timer loop (pause/hidden-tab/backoff/overlap generations), tile/section rendering per §5 | `crates/dbc-ui/src/monitor_view.rs` (new), `crates/dbc-ui/src/tabs.rs`, `crates/dbc-ui/src/main.rs` | T4 | Unit: `monitor_available`, duration-tier colour selection as a pure `fn duration_tier(secs: f64) -> Tier`. GPUI rendering itself is not unit-tested anywhere else in this repo either — visual check via `/run` skill against a docker pg instance |
| T6 | Kill confirm dialog (`ModalState::KillConfirm`) + wiring | `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/monitor_view.rs`, `crates/dbc-ui/src/main.rs` | T4, T5 | Unit: the pid-formatting/SQL-string-building helper (pure); manual/`/run`-skill check for the modal itself |
| T7 | Docker integration test(s) proving T2's SQL against a real Postgres, including a deliberate lock wait to exercise the blocking-chain query | `crates/dbc-ui/tests/monitor_postgres.rs` (new) | T1, T2, T4 | `#[ignore]`d, `testcontainers_modules::postgres` — same pattern as `crates/dbc-driver-postgres/tests/integration.rs`; run via `cargo test -p dbc-ui -- --ignored`. Two testcontainer sessions: one holds a row lock, the other blocks on it, asserting the blocking-chain query returns the expected waiter/blocker pair and `build_blocking_tree` nests them correctly |

**Parallelization:** T1, T2, T3 are independent pure-Rust work, can run
fully in parallel. T4 needs T1+T2 (not T3 — MSSQL path is unreachable
today). T5 needs T4. T6 needs T4 (for `MonitorCmd::Kill` to exist) and can
be built alongside T5's tile rendering once that's true. T7 needs
T1+T2+T4 and can run in parallel with T5/T6.

## 9. Risks / needs-verification

1. **Highest risk:** the kill read-only gate is app-level-only (§0) — no
   server-side backstop on either engine. A bug in `MonitorView`'s
   disabled-state check or the background task's `read_only` capture is a
   real "read-only connection can still kill sessions" hole, not a
   cosmetic one. CURATION: the T6 test asserting the background task
   refuses `Kill` when opened with `read_only: true` (independent of
   whatever the UI renders) is REQUIRED, not recommended — the app-level
   flag is the only enforcement, so it gets guard-level test discipline
   (same class as `guards.rs`).
2. **MSSQL SQL is unverified** — no driver, no server, in this repo. T3's
   queries are best-effort from DMV documentation, not proven against a
   live SQL Server. Flag for a follow-up pass once `dbc-driver-mssql`
   lands (orthogonal roadmap item, currently unscheduled).
3. **"Today's deadlock count" doesn't exist natively** on either engine —
   both expose a since-last-reset/since-restart cumulative counter
   instead. This design relabels the tile rather than faking a calendar-day
   count via a locally-tracked midnight baseline (rejected as unjustified
   complexity for a monitoring nicety) — worth confirming against the
   mockup's actual wording/expectation before implementation, since the
   spec text says "today's".
4. **Permission-dependent partial data** (§2/§3 caveats): a low-privilege
   connecting role sees nulled/partial rows on Postgres or empty/
   permission-denied on MSSQL DMVs. Every per-query failure inside a
   refresh must degrade that ONE tile/section to "n/a", not fail the whole
   snapshot — T4/T7 should explicitly test a partial-failure refresh
   (one query errors, others succeed) still produces a renderable
   `MonitorSnapshot`.
5. **MSSQL running-queries query cost** (`CROSS APPLY sys.dm_exec_sql_text`
   per session) is a known-relatively-expensive pattern on servers with
   very large session counts; `TOP 200` bounds it but hasn't been
   load-tested (no driver to test against yet) — revisit once T3 becomes
   runnable.
6. **Multiple simultaneous monitor tabs** (different connections open at
   once) should work by construction (each `MonitorView`/background task
   pair is fully independent, no shared state) but T7 only exercises a
   single connection — flag as untested-but-expected-to-work.
7. **Idle-forever background task after entity drop:** §4 notes the
   `cx.spawn` timer loop, if GPUI ever delivers a dangling-entity update as
   "silently return the old value" rather than an error/`None`, could in
   theory keep sleeping-and-no-op'ing forever instead of stopping. Low
   severity (cheap no-op, not a leak of the connection — that's already
   torn down via the dropped `Sender`), but worth a `this.upgrade().
   is_none() => break` explicit check during T5 review rather than relying
   on `unwrap_or(false)`'s behavior being what's assumed here.
