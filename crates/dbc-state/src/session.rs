//! „Otevři se tak, jak jsem to zavřel" — the window state that survives a
//! restart (user request 2026-08-31).
//!
//! # What is stored, and what is not
//!
//! Names and SQL: which connection was active, which database, which tree
//! rows were open, the editor's text, and the TITLE AND SQL of the open
//! tabs. That is the same class of thing `history.sqlite` has always held —
//! every statement you have run, with its connection name — so this file
//! moves no boundary.
//!
//! NOT the editor's script binding. The file on disk is the truth for a
//! bound script, so restoring an in-memory copy while still claiming the
//! binding would lie about what the next Ctrl+S overwrites. Unsaved text
//! comes back unbound instead, which makes saving it ask where — the safe
//! answer, and the one that cannot quietly overwrite someone's file.
//!
//! It holds NO result data and NO secret. Not a password, not a connection
//! string, not one row of anything a query returned. A tab comes back as
//! the query that made it, never as the answer; re-running is a click. That
//! is not a limitation to be worked around later, it is the rule: the vault
//! is the only file in this app that may hold a secret, and no file at all
//! may hold result data.
//!
//! # Where it lives, and why not next to `config.toml`
//!
//! Always machine-local, in the profile directory, keyed by which context
//! it belongs to — never inside a workspace folder. Two reasons, and the
//! codebase already settled both: a workspace folder can be in git or on a
//! share (§W5 made `history.sqlite` machine-local for exactly this), and
//! „my open tabs" is not something to hand a colleague along with the
//! connection list. Keying by context additionally means switching
//! workspaces cannot restore the other one's active connection — an id that
//! may not even exist there.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Tabs are capped at 10 by `dbc-ui`'s `TAB_CAP`; this is the same bound
/// restated where the file is written, so a corrupt or hand-edited session
/// cannot make startup open an unbounded number of tabs.
const MAX_TABS: usize = 16;

/// A single SQL text this file will carry. Past this the text is DROPPED,
/// not truncated: half a statement restored into the editor looks like
/// work that survived when it did not.
const MAX_SQL_BYTES: usize = 1024 * 1024;

/// Editor tabs per session (2026-09-02). Matches `dbc-ui`'s
/// `editor_tabs::MAX_EDITOR_TABS`; a file naming more is a file written by
/// something else, and the extra tabs are dropped rather than trusted.
pub const MAX_EDITORS: usize = 32;

/// Enough for a deeply browsed schema, far short of „every table in a
/// 1171-table database is open".
const MAX_EXPANDED_NODES: usize = 2000;

/// One restored tab: what it was called and what query made it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTab {
    pub title: String,
    pub sql: String,
    #[serde(default)]
    pub pinned: bool,
}

/// The whole restorable window state.
///
/// Every field is a name, a path, an offset or SQL. If a future field is
/// none of those, it does not belong in this struct — see the module doc.
/// One editor tab (spec §3 of the 2026-09-02 editor-tabs design). `tabs`
/// are THIS editor's result tabs — the same shape the legacy top-level
/// `tabs` had for the one editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEditor {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sql: String,
    #[serde(default)]
    pub cursor: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// The bound `.sql` file, absolute — same meaning as the binding's
    /// `path` in the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<SessionTab>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// `ConnectionConfig::id` of the active connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    /// The database within it, when it is not the connection's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// The SQL editor's text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub editor: String,
    /// Byte offset of the cursor in `editor`. Clamped on restore, so a
    /// stale or hand-edited value cannot panic the editor.
    #[serde(default)]
    pub cursor: usize,
    /// Encoded expanded sidebar rows — `dbc-ui` owns the encoding, this
    /// module only carries the strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<String>,
    /// The expand state INSIDE each loaded database — sections, tables,
    /// columns. A separate list because it lives somewhere else in the UI
    /// (one set per loaded snapshot, not one flat set), and because it can
    /// only be applied once that database's schema has arrived.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanded_nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<SessionTab>,
    /// Editor tabs (2026-09-02). The legacy `connection`/`database`/
    /// `editor`/`cursor`/`tabs` above are STILL WRITTEN, for the active
    /// editor, so a build from before editor tabs opens with that one
    /// intact; on load they are only read when this is empty
    /// (`editors_or_legacy`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editors: Vec<SessionEditor>,
    /// Skipped at 0 so an untouched session still serialises to the same
    /// single `cursor = 0` line it always did (`is_empty` deletes it).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_editor: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl SessionState {
    /// The editors to open: `editors` when present, else the legacy fields
    /// as one editor — and never none, because the app always has a tab.
    pub fn editors_or_legacy(&self) -> (Vec<SessionEditor>, usize) {
        if !self.editors.is_empty() {
            return (self.editors.clone(), self.active_editor.min(self.editors.len() - 1));
        }
        let one = SessionEditor {
            sql: self.editor.clone(),
            cursor: self.cursor,
            connection: self.connection.clone(),
            database: self.database.clone(),
            script_path: None,
            tabs: self.tabs.clone(),
        };
        (vec![one], 0)
    }

    /// Drop anything oversized or over-numerous. Applied on both save and
    /// load, so neither a runaway app nor a hand-edited file can produce a
    /// session that misbehaves on startup.
    pub fn clamped(mut self) -> Self {
        if self.editor.len() > MAX_SQL_BYTES {
            self.editor = String::new();
            self.cursor = 0;
        }
        self.cursor = self.cursor.min(self.editor.len());
        while !self.editor.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        self.tabs.retain(|t| t.sql.len() <= MAX_SQL_BYTES);
        self.tabs.truncate(MAX_TABS);
        // A tree opened all the way down a 1171-table database would
        // otherwise write a megabyte of node ids. Past the cap the inner
        // expansion is dropped whole: losing „which tables were open" is a
        // small, obvious loss, where a half-restored tree is a confusing
        // one.
        if self.expanded_nodes.len() > MAX_EXPANDED_NODES {
            self.expanded_nodes.clear();
        }
        // The same three rules the legacy fields get, once per editor.
        self.editors.truncate(MAX_EDITORS);
        for e in &mut self.editors {
            if e.sql.len() > MAX_SQL_BYTES {
                e.sql = String::new();
                e.cursor = 0;
            }
            e.cursor = e.cursor.min(e.sql.len());
            while !e.sql.is_char_boundary(e.cursor) {
                e.cursor -= 1;
            }
            e.tabs.retain(|t| t.sql.len() <= MAX_SQL_BYTES);
            e.tabs.truncate(MAX_TABS);
        }
        if !self.editors.is_empty() {
            self.active_editor = self.active_editor.min(self.editors.len() - 1);
        }
        self
    }

    /// Nothing worth writing — used to remove the file rather than leave a
    /// stale one behind.
    pub fn is_empty(&self) -> bool {
        *self == SessionState::default()
    }
}

/// FNV-1a over the context's `config.toml` path — the same shape
/// `schema_cache` uses, and for the same two reasons: a filename that is
/// legal on every filesystem, and a profile directory whose listing does
/// not advertise which folders someone works in.
fn key(config_path: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in config_path.to_string_lossy().to_lowercase().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}.toml")
}

fn dir() -> PathBuf {
    crate::workspace::profile_dir().join("sessions")
}

/// The session file for the context identified by `config_path`.
pub fn path_for(config_path: &Path) -> PathBuf {
    dir().join(key(config_path))
}

/// The stored session, or an empty one.
///
/// Takes the SESSION file's own path (from [`path_for`]), not the config
/// path it was derived from. Two reasons: the FNV hash is then computed
/// once at startup rather than on every save, and — the one that decides
/// it — `dbc-ui`'s `config_save_guard_audit` treats any `save(` line that
/// also names `config_path` as a write to `config.toml`, which this is
/// emphatically not. Making the two shapes textually different is better
/// than teaching the audit an exception.
pub fn load(session_path: &Path) -> SessionState {
    let Ok(text) = std::fs::read_to_string(session_path) else {
        return SessionState::default();
    };
    toml::from_str::<SessionState>(&text).unwrap_or_default().clamped()
}

/// Best-effort write. A session that cannot be saved costs a restart that
/// opens blank; it must never cost a failed shutdown, so every error here
/// is swallowed.
pub fn save(session_path: &Path, state: &SessionState) {
    let state = state.clone().clamped();
    if state.is_empty() {
        let _ = std::fs::remove_file(session_path);
        return;
    }
    if let Some(parent) = session_path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(text) = toml::to_string_pretty(&state) else { return };
    let _ = crate::fsutil::write_atomic(session_path, text.as_bytes());
}

/// Forget this context's session.
pub fn clear(session_path: &Path) {
    let _ = std::fs::remove_file(session_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editors_round_trip_through_toml() {
        let s = SessionState {
            editors: vec![
                SessionEditor {
                    sql: "select 1".into(),
                    cursor: 3,
                    connection: Some("c1".into()),
                    database: Some("sales".into()),
                    script_path: Some(PathBuf::from("D:/x/report.sql")),
                    tabs: vec![SessionTab { title: "t".into(), sql: "select 1".into(), pinned: true }],
                },
                SessionEditor { sql: "select 2".into(), ..Default::default() },
            ],
            active_editor: 1,
            ..Default::default()
        };
        let text = toml::to_string(&s).unwrap();
        let back: SessionState = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    /// A session written before editors existed becomes editor 0 — and
    /// nothing is lost: text, cursor, context and the result tabs all move.
    #[test]
    fn a_legacy_session_becomes_a_single_editor() {
        let legacy = SessionState {
            connection: Some("c1".into()),
            database: None,
            editor: "select 1".into(),
            cursor: 2,
            tabs: vec![SessionTab { title: "t".into(), sql: "select 1".into(), pinned: false }],
            ..Default::default()
        };
        let (editors, active) = legacy.editors_or_legacy();
        assert_eq!(active, 0);
        assert_eq!(editors.len(), 1);
        assert_eq!(editors[0].sql, "select 1");
        assert_eq!(editors[0].cursor, 2);
        assert_eq!(editors[0].connection.as_deref(), Some("c1"));
        assert_eq!(editors[0].tabs.len(), 1);
    }

    /// An EMPTY legacy session still yields one (empty) editor: the app
    /// always has a tab.
    #[test]
    fn an_empty_session_still_yields_one_editor() {
        let (editors, active) = SessionState::default().editors_or_legacy();
        assert_eq!((editors.len(), active), (1, 0));
        assert!(editors[0].sql.is_empty());
    }

    #[test]
    fn when_editors_exist_the_legacy_fields_are_ignored() {
        let s = SessionState {
            editor: "legacy".into(),
            editors: vec![SessionEditor { sql: "new".into(), ..Default::default() }],
            active_editor: 0,
            ..Default::default()
        };
        let (editors, _) = s.editors_or_legacy();
        assert_eq!(editors[0].sql, "new");
    }

    #[test]
    fn clamped_caps_editors_and_their_tabs_and_the_active_index() {
        let mut s = SessionState::default();
        for _ in 0..(MAX_EDITORS + 5) {
            s.editors.push(SessionEditor {
                sql: "x".into(),
                tabs: (0..(MAX_TABS + 3)).map(|_| SessionTab::default()).collect(),
                ..Default::default()
            });
        }
        s.active_editor = 999;
        let s = s.clamped();
        assert_eq!(s.editors.len(), MAX_EDITORS);
        assert!(s.editors.iter().all(|e| e.tabs.len() <= MAX_TABS));
        assert_eq!(s.active_editor, MAX_EDITORS - 1);
    }

    #[test]
    fn clamped_drops_an_oversized_editor_buffer_and_fixes_its_cursor() {
        let mut s = SessionState::default();
        s.editors.push(SessionEditor {
            sql: "a".repeat(MAX_SQL_BYTES + 1),
            cursor: 5,
            ..Default::default()
        });
        // Byte 2 is inside 'ž' (two bytes) — not a char boundary.
        s.editors.push(SessionEditor { sql: "žluť".into(), cursor: 2, ..Default::default() });
        let s = s.clamped();
        assert!(s.editors[0].sql.is_empty());
        assert_eq!(s.editors[0].cursor, 0);
        assert!(s.editors[1].sql.is_char_boundary(s.editors[1].cursor));
    }

    fn full() -> SessionState {
        SessionState {
            editors: vec![
                SessionEditor {
                    sql: "SELECT 1".into(),
                    cursor: 3,
                    connection: Some("conn-1".into()),
                    database: Some("dw".into()),
                    script_path: Some(PathBuf::from("D:/scripts/report.sql")),
                    tabs: vec![SessionTab { title: "t".into(), sql: "SELECT 1".into(), pinned: false }],
                },
                SessionEditor { sql: "SELECT 2".into(), ..Default::default() },
            ],
            active_editor: 1,
            connection: Some("conn-1".into()),
            database: Some("dw".into()),
            editor: "SELECT 1".into(),
            cursor: 3,
            expanded: vec!["conn:conn-1".into()],
            expanded_nodes: vec!["conn-1|dw|t|dbo|orders".into()],
            tabs: vec![SessionTab {
                title: "Náhled: orders".into(),
                sql: "SELECT * FROM orders".into(),
                pinned: true,
            }],
        }
    }

    /// THE rule, pinned as a shape test: the serialized session may contain
    /// only these keys. A future field that is a secret or a row of data
    /// fails here before it can reach anyone's disk.
    #[test]
    fn the_file_carries_only_names_sql_and_offsets() {
        const ALLOWED: &[&str] = &[
            "connection", "database", "editor", "cursor", "expanded", "expanded_nodes", "title",
            "sql", "pinned",
            // Editor tabs (2026-09-02): the per-editor record repeats the
            // names above plus the bound script's PATH — a file name, never
            // its contents.
            "active_editor", "script_path",
        ];
        let text = toml::to_string_pretty(&full()).unwrap();
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split('=').next())
            .map(str::trim)
            .filter(|k| !k.is_empty() && !k.starts_with('[') && !k.starts_with('#'))
            .collect();
        assert!(keys.len() >= 7, "the scan found almost nothing: {keys:?}");
        for k in &keys {
            assert!(ALLOWED.contains(k), "unexpected key {k:?} in the session file:\n{text}");
        }
        // …and the allowlist is the thing to be careful with, so it is
        // checked too. Substring, not equality: `password_hash` and
        // `master_secret` must be as unwelcome as the bare words. Matched
        // against the ALLOWLIST rather than the rendered file, because a
        // legitimate value („database = dw") contains „data" and a
        // whole-text scan would fail on it while catching nothing real.
        for name in ALLOWED {
            for forbidden in ["password", "secret", "heslo", "token", "credential"] {
                assert!(!name.contains(forbidden), "{name:?} does not belong in a session file");
            }
        }
    }

    #[test]
    fn roundtrips_through_toml() {
        let text = toml::to_string_pretty(&full()).unwrap();
        assert_eq!(toml::from_str::<SessionState>(&text).unwrap(), full());
    }

    #[test]
    fn an_empty_session_serializes_to_nothing() {
        assert_eq!(toml::to_string_pretty(&SessionState::default()).unwrap().trim(), "cursor = 0");
        assert!(SessionState::default().is_empty());
    }

    /// Half a statement restored into the editor would look like work that
    /// survived, so an oversized buffer is dropped whole.
    #[test]
    fn an_oversized_editor_is_dropped_not_truncated() {
        let s = SessionState { editor: "x".repeat(MAX_SQL_BYTES + 1), cursor: 99, ..Default::default() }
            .clamped();
        assert!(s.editor.is_empty());
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn an_oversized_tab_is_dropped_and_the_rest_kept() {
        let s = SessionState {
            tabs: vec![
                SessionTab { title: "ok".into(), sql: "SELECT 1".into(), pinned: false },
                SessionTab { title: "huge".into(), sql: "x".repeat(MAX_SQL_BYTES + 1), pinned: false },
            ],
            ..Default::default()
        }
        .clamped();
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].title, "ok");
    }

    #[test]
    fn too_many_tabs_are_capped() {
        let s = SessionState {
            tabs: (0..100)
                .map(|i| SessionTab { title: i.to_string(), sql: "SELECT 1".into(), pinned: false })
                .collect(),
            ..Default::default()
        }
        .clamped();
        assert_eq!(s.tabs.len(), MAX_TABS);
    }

    /// A hand-edited or stale cursor must not be able to panic the editor.
    #[test]
    fn the_cursor_is_clamped_to_a_char_boundary() {
        let s = SessionState { editor: "SELECT 'č'".into(), cursor: 999, ..Default::default() }.clamped();
        assert_eq!(s.cursor, s.editor.len());
        let mid = SessionState { editor: "č".into(), cursor: 1, ..Default::default() }.clamped();
        assert_eq!(mid.cursor, 0, "byte 1 is inside the two-byte 'č'");
    }

    #[test]
    fn save_and_load_round_trip_through_a_real_file() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("s.toml");
        save(&p, &full());
        assert_eq!(load(&p), full());
        clear(&p);
        assert_eq!(load(&p), SessionState::default(), "a missing file is an empty session");
    }

    #[test]
    fn a_corrupt_file_reads_as_an_empty_session() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("s.toml");
        std::fs::write(&p, "this is not toml at all {{{").unwrap();
        assert_eq!(load(&p), SessionState::default());
    }

    /// Saving an empty session REMOVES the file rather than leaving a
    /// stale one that would resurrect last week's tabs.
    #[test]
    fn saving_an_empty_session_removes_the_file() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("s.toml");
        save(&p, &full());
        assert!(p.exists());
        save(&p, &SessionState::default());
        assert!(!p.exists());
    }

    /// Two contexts must not share one session: restoring workspace A's
    /// active connection while workspace B is open would name an id that
    /// need not exist there.
    #[test]
    fn each_context_gets_its_own_file() {
        let a = Path::new("D:\\ws\\alfa\\config.toml");
        let b = Path::new("D:\\ws\\beta\\config.toml");
        assert_ne!(path_for(a), path_for(b));
        assert_eq!(path_for(a), path_for(a));
    }

    #[test]
    fn the_key_is_a_plain_filename_whatever_the_path_looks_like() {
        for p in ["C:\\a b\\c:d\\config.toml", "/tmp/../x/config.toml", ""] {
            let k = key(Path::new(p));
            assert!(k.ends_with(".toml"));
            assert!(!k.contains('/') && !k.contains('\\') && !k.contains(':'));
        }
    }
}
