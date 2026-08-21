# Follow-ups from phases 0-2 final review (2026-08-21)

Carried out of the merged branch `feature/phases-0-2`. Source: final whole-branch
review + per-task review ledger (workspace deleted after merge; this file is the
durable record).

## Should fix early in phase 3

- **I1 — byte-based buffer cap missing (spec deviation).** Spec §4 says
  "500k rows / 256 MB"; only the row cap exists. Wide rows can reach ~1 GB
  before spilling. Fix: accumulate `batch.get_array_memory_size()` against a
  byte cap in `dbc-buffer`, spill on either limit.
- **I2 — spill I/O runs on the UI thread and panics on failure.** Both the
  write path (`ResultBuffer::push` called from the Batch event arm) and the
  read path (`slot_batch` expects) can crash the app on disk-full/AV-locked
  temp files. Make spill fallible (`push -> Result`, graceful
  `"<spill read error>"` cell fallback) and consider moving spill writes off
  the UI thread.
- **I4 — per-query `connect::open` with `block_on` on the UI thread** freezes
  the window for the full TCP timeout on unreachable hosts, per keypress.
  Accepted for phase 2 by ruling; phase 4 (connection manager) must fix it.

## Nice to have

- M1: SQLite driver has no 16 ms latency flush (only BATCH_ROWS); align with
  the constants' documented contract or scope the contract to Postgres.
- M4: Postgres cancel cannot reach the `prepare()` phase (watcher spawns after
  prepare succeeds); UI cancel state desyncs if prepare blocks on a DDL lock.
- M5: `QueryError.position` doc says byte offset; Postgres reports 1-based
  character position. Fix doc now, convert units in the phase-5 editor.
- M6: `rust-toolchain.toml` `channel = "stable"` floats; pin a version if
  reproducibility is wanted.
- M8: `connect::open` treats any non-pg string as a SQLite path and
  `rusqlite::open` creates missing files — typos silently create empty DBs.
- M9 (grab-bag): duplicate `tempfile` dep in dbc-buffer (deps + dev-deps);
  unused `anyhow` workspace dep; both drivers' `schema()` silently drop
  undecodable rows via `filter_map(Result::ok)`; status-bar elapsed doesn't
  tick during quiet long queries; CRLF paste can leave `\r` in SQL (inherited
  from Zed input example); `started_at` never reset to `None`; trailing `\n`
  after last TSV row in grid copy; resize drag tracking is grid-bounds-scoped
  (sticky if cursor overshoots; `mouse_up_out` terminates safely).
- Test gap worth closing before MSSQL driver work: a fake-driver test at the
  `runner.rs` seam (spec §7 named it; dbc-core half was consciously skipped,
  the runner half is where it would pay off).

## Deferred human verification

- Visual/interactive pass: grid rendering + scroll, typing + Ctrl+Enter + Esc,
  drag resize, click/shift-click selection, Ctrl+C paste, and the 1M-row
  live-Postgres scroll check
  (`SELECT g AS id, md5(g::text) AS hash FROM generate_series(1, 1000000) g`
  in `--release`).
