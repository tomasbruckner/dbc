use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::StateError;

/// Consecutive re-runs of the same (sql, connection) within this window are
/// collapsed into the previous entry instead of creating a new row.
const DEDUP_WINDOW_SECS: i64 = 5;

fn err(m: impl Into<String>) -> StateError {
    StateError { message: m.into() }
}

impl From<rusqlite::Error> for StateError {
    fn from(e: rusqlite::Error) -> Self {
        StateError { message: e.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub sql: String,
    pub connection: String, // connection NAME, never a URL/credentials
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub error: Option<String>, // failed runs recorded with the error text
    pub starred: bool,
    /// G11 T7: additive. `"query"` for every ordinary run (the column's own
    /// `DEFAULT 'query'` covers rows written before this migration, and
    /// `add`'s thin wrapper covers every row written after it via `add`);
    /// `"backup"`/`"restore"` for G11 runs recorded via `add_with_kind`.
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Fts5,
    Like,
}

pub struct HistoryDb {
    conn: Connection,
    mode: SearchMode,
}

impl HistoryDb {
    /// Opens/creates the DB and migrates the schema. Never panics.
    pub fn open(path: &Path) -> Result<HistoryDb, StateError> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entries (
                id INTEGER PRIMARY KEY,
                sql TEXT NOT NULL,
                connection TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                duration_ms INTEGER,
                row_count INTEGER,
                error TEXT,
                starred INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_entries_star_time
                ON entries(starred DESC, started_at DESC);",
        )?;

        // G11 T7: additive `kind` column — SQLite has no `ADD COLUMN IF NOT
        // EXISTS`, so idempotency comes from probing `PRAGMA table_info`
        // first, same "probe, then conditionally migrate" shape the star/
        // time index above already proves works (generalized from an index
        // to a column). `DEFAULT 'query'` covers every pre-G11 row without
        // a separate UPDATE pass.
        let has_kind_column: bool = conn
            .prepare("PRAGMA table_info(entries)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == "kind");
        if !has_kind_column {
            conn.execute_batch("ALTER TABLE entries ADD COLUMN kind TEXT NOT NULL DEFAULT 'query';")?;
        }

        // Detect FTS5 availability by attempting to create the external-content
        // table + sync triggers. Some SQLite builds are compiled without FTS5,
        // so this must degrade gracefully to a LIKE-based fallback.
        let mode = match conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts
                USING fts5(sql, content='entries', content_rowid='id');
             CREATE TRIGGER IF NOT EXISTS entries_fts_ai AFTER INSERT ON entries BEGIN
                INSERT INTO entries_fts(rowid, sql) VALUES (new.id, new.sql);
             END;
             CREATE TRIGGER IF NOT EXISTS entries_fts_ad AFTER DELETE ON entries BEGIN
                INSERT INTO entries_fts(entries_fts, rowid, sql) VALUES('delete', old.id, old.sql);
             END;
             CREATE TRIGGER IF NOT EXISTS entries_fts_au AFTER UPDATE ON entries BEGIN
                INSERT INTO entries_fts(entries_fts, rowid, sql) VALUES('delete', old.id, old.sql);
                INSERT INTO entries_fts(rowid, sql) VALUES (new.id, new.sql);
             END;",
        ) {
            Ok(()) => SearchMode::Fts5,
            Err(_) => SearchMode::Like,
        };

        Ok(HistoryDb { conn, mode })
    }

    /// Returns the new entry id. Existing 6-argument signature UNCHANGED —
    /// every current call site keeps compiling with zero edits (G11 T7: now
    /// a thin `kind = "query"` wrapper around `add_with_kind`).
    pub fn add(
        &mut self,
        sql: &str,
        connection: &str,
        started_at: i64,
        duration_ms: Option<i64>,
        row_count: Option<i64>,
        error: Option<&str>,
    ) -> Result<i64, StateError> {
        self.add_with_kind(sql, connection, started_at, duration_ms, row_count, error, "query")
    }

    /// G11 T7: the ONLY way a non-`"query"` `kind` ever reaches the DB
    /// (`"backup"`/`"restore"` for G11 runs). Dedup check UNCHANGED in
    /// shape (still keys on sql+connection+time window — a backup/restore's
    /// synthetic "sql" description is already unique-per-run via its
    /// embedded timestamped file path, so this never spuriously collapses
    /// two different runs); the UPDATE/INSERT additionally write `kind`.
    pub fn add_with_kind(
        &mut self,
        sql: &str,
        connection: &str,
        started_at: i64,
        duration_ms: Option<i64>,
        row_count: Option<i64>,
        error: Option<&str>,
        kind: &str,
    ) -> Result<i64, StateError> {
        let last: Option<(i64, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT id, sql, connection, started_at FROM entries ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        if let Some((last_id, last_sql, last_conn, last_started_at)) = last {
            if last_sql == sql
                && last_conn == connection
                && (started_at - last_started_at).abs() <= DEDUP_WINDOW_SECS
            {
                self.conn.execute(
                    "UPDATE entries SET started_at = ?1, duration_ms = ?2, row_count = ?3, error = ?4, kind = ?5
                     WHERE id = ?6",
                    params![started_at, duration_ms, row_count, error, kind, last_id],
                )?;
                return Ok(last_id);
            }
        }

        self.conn.execute(
            "INSERT INTO entries (sql, connection, started_at, duration_ms, row_count, error, starred, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            params![sql, connection, started_at, duration_ms, row_count, error, kind],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// query empty → recent entries; otherwise FTS/LIKE fulltext over sql.
    /// Starred entries first (both modes), then newest first. Max `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>, StateError> {
        if query.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT id, sql, connection, started_at, duration_ms, row_count, error, starred, kind
                 FROM entries
                 ORDER BY starred DESC, started_at DESC, id DESC
                 LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], row_to_entry)?
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(rows);
        }

        match self.mode {
            SearchMode::Fts5 => {
                let mut stmt = self.conn.prepare(
                    "SELECT e.id, e.sql, e.connection, e.started_at, e.duration_ms, e.row_count, e.error, e.starred, e.kind
                     FROM entries e
                     JOIN entries_fts f ON f.rowid = e.id
                     WHERE entries_fts MATCH ?1
                     ORDER BY e.starred DESC, e.started_at DESC, e.id DESC
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![fts_phrase(query), limit as i64], row_to_entry)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
            SearchMode::Like => {
                let pattern = format!("%{}%", like_escape(query));
                let mut stmt = self.conn.prepare(
                    "SELECT id, sql, connection, started_at, duration_ms, row_count, error, starred, kind
                     FROM entries
                     WHERE sql LIKE ?1 ESCAPE '\\'
                     ORDER BY starred DESC, started_at DESC, id DESC
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![pattern, limit as i64], row_to_entry)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
        }
    }

    pub fn set_starred(&mut self, id: i64, starred: bool) -> Result<(), StateError> {
        let n = self.conn.execute(
            "UPDATE entries SET starred = ?1 WHERE id = ?2",
            params![starred as i64, id],
        )?;
        if n == 0 {
            return Err(err(format!("history entry {id} not found")));
        }
        Ok(())
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        sql: row.get(1)?,
        connection: row.get(2)?,
        started_at: row.get(3)?,
        duration_ms: row.get(4)?,
        row_count: row.get(5)?,
        error: row.get(6)?,
        starred: row.get::<_, i64>(7)? != 0,
        kind: row.get(8)?,
    })
}

/// Wraps a user search string as an FTS5 *prefix* phrase query so special
/// MATCH syntax characters in `q` are treated as literal text, not query
/// syntax, AND partial words typed so far still match (live search re-runs
/// on every keystroke — a whole-token-only phrase query like `"ord"` finds
/// nothing against `orders` until the whole word is typed; `"ord"*` matches
/// as a prefix). The `*` sits outside the closing quote so it applies to
/// the phrase as a prefix operator rather than becoming literal text inside
/// it; quote-doubling for literal `"` in `q` is unaffected.
fn fts_phrase(q: &str) -> String {
    format!("\"{}\"*", q.replace('"', "\"\""))
}

/// Escapes `%`, `_`, and the escape character itself for use inside a
/// `LIKE ... ESCAPE '\'` pattern.
fn like_escape(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for c in q.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

pub fn default_history_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dbc")
        .join("history.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> (tempfile::TempDir, HistoryDb) {
        let d = tempfile::tempdir().unwrap();
        let h = HistoryDb::open(&d.path().join("h.sqlite")).unwrap();
        (d, h)
    }

    #[test]
    fn add_and_recent() {
        let (_d, mut h) = db();
        h.add("select 1", "demo", 1000, Some(5), Some(1), None).unwrap();
        h.add("select 2", "demo", 2000, Some(6), Some(1), None).unwrap();
        let r = h.search("", 10).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].sql, "select 2"); // newest first
    }

    #[test]
    fn fulltext_finds_and_misses() {
        let (_d, mut h) = db();
        h.add("select * from orders where id = 1", "demo", 1000, None, None, None).unwrap();
        h.add("update inventory set qty = 0", "demo", 2000, None, None, None).unwrap();
        let r = h.search("orders", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].sql.contains("orders"));
        assert!(h.search("nonexistent_zzz", 10).unwrap().is_empty());
    }

    #[test]
    fn fulltext_matches_on_partial_word_prefix() {
        // G3 final-review regression (F2): live search re-queries on every
        // keystroke, so a partial word typed so far ("ord") must already
        // match "orders" — a whole-token-only phrase query finds nothing
        // until the full word is typed, which broke the live-search UX.
        let (_d, mut h) = db();
        h.add("select * from orders where id = 1", "demo", 1000, None, None, None).unwrap();
        h.add("update inventory set qty = 0", "demo", 2000, None, None, None).unwrap();
        let r = h.search("ord", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].sql.contains("orders"));
    }

    #[test]
    fn starred_first_and_persists() {
        let (d, mut h) = db();
        let a = h.add("aaa", "demo", 1000, None, None, None).unwrap();
        h.add("bbb", "demo", 2000, None, None, None).unwrap();
        h.set_starred(a, true).unwrap();
        let r = h.search("", 10).unwrap();
        assert!(r[0].starred && r[0].sql == "aaa");
        drop(h);
        let h2 = HistoryDb::open(&d.path().join("h.sqlite")).unwrap();
        assert!(h2.search("", 10).unwrap()[0].starred); // survives reopen
    }

    #[test]
    fn consecutive_dedup_within_window() {
        let (_d, mut h) = db();
        h.add("select 1", "demo", 1000, Some(5), Some(1), None).unwrap();
        h.add("select 1", "demo", 1003, Some(4), Some(1), None).unwrap(); // within 5 s
        h.add("select 1", "demo", 2000, Some(4), Some(1), None).unwrap(); // outside
        assert_eq!(h.search("", 10).unwrap().len(), 2);
    }

    #[test]
    fn failed_run_recorded_with_error() {
        let (_d, mut h) = db();
        h.add("select bad", "demo", 1000, None, None, Some("syntax error")).unwrap();
        let r = h.search("", 10).unwrap();
        assert_eq!(r[0].error.as_deref(), Some("syntax error"));
    }

    /// Review Issue 1: the empty-search hot path sorts by
    /// `(starred DESC, started_at DESC)` with no index, forcing a full-table
    /// scan+sort on every call. `idx_entries_star_time` must exist after
    /// `open`, both freshly and on a pre-existing DB opened again (the
    /// `CREATE INDEX IF NOT EXISTS` migration must be idempotent).
    #[test]
    fn open_creates_the_star_time_index_and_reopen_is_idempotent() {
        let (d, h) = db();
        let has_index = |h: &HistoryDb| -> bool {
            h.conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_entries_star_time'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .unwrap()
                .is_some()
        };
        assert!(has_index(&h));
        drop(h);
        let h2 = HistoryDb::open(&d.path().join("h.sqlite")).unwrap();
        assert!(has_index(&h2));
    }

    // --- G11 T7: kind column ---
    #[test]
    fn add_defaults_to_query_kind() {
        let (_d, mut h) = db();
        h.add("select 1", "demo", 1000, None, None, None).unwrap();
        assert_eq!(h.search("", 10).unwrap()[0].kind, "query");
    }

    #[test]
    fn add_with_kind_records_backup_and_restore() {
        let (_d, mut h) = db();
        h.add_with_kind("-- BACKUP demo -> x.backup", "demo", 1000, Some(5000), None, None, "backup").unwrap();
        h.add_with_kind("-- RESTORE demo <- x.backup", "demo", 2000, Some(3000), None, None, "restore").unwrap();
        let r = h.search("", 10).unwrap();
        assert_eq!(r[0].kind, "restore"); // newest first
        assert_eq!(r[1].kind, "backup");
    }

    #[test]
    fn old_db_without_kind_column_migrates_on_reopen() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("h.sqlite");
        {
            // Simulate a pre-G11 DB: create the OLD schema directly,
            // bypassing HistoryDb::open (which would already add the
            // column).
            let conn = rusqlite::Connection::open(&p).unwrap();
            conn.execute_batch(
                "CREATE TABLE entries (
                    id INTEGER PRIMARY KEY, sql TEXT NOT NULL, connection TEXT NOT NULL,
                    started_at INTEGER NOT NULL, duration_ms INTEGER, row_count INTEGER,
                    error TEXT, starred INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO entries (sql, connection, started_at, starred) VALUES ('select 1', 'demo', 1000, 0);",
            )
            .unwrap();
        }
        let h = HistoryDb::open(&p).unwrap();
        let r = h.search("", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].kind, "query", "pre-existing rows must default to 'query' via the column's DEFAULT");
    }

    #[test]
    fn kind_column_migration_is_idempotent_on_reopen() {
        let (d, h) = db();
        drop(h);
        // Reopening an ALREADY-migrated (this session's own fresh) DB a
        // second time must not error on a duplicate ALTER TABLE.
        let h2 = HistoryDb::open(&d.path().join("h.sqlite"));
        assert!(h2.is_ok());
    }
}
