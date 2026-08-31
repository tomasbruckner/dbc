//! The diagnostic log — a plain text file next to the profile.
//!
//! Written because the app had no way to answer „kliknul jsem a nic se
//! nestalo" (user report, 2026-08-29: an ER diagram opened blank and the
//! only trace was a status line that had already been replaced).
//!
//! # What may be logged
//!
//! [`Event`] is a CLOSED vocabulary, and that is the whole design. There is
//! no `log!("{}", anything)` — adding a new thing to the log means adding a
//! variant here, in one file, where the question „does this field carry user
//! data?" is asked once and stays answered. It is not a compiler-enforced
//! rail (a `String` field can still be handed the wrong string) but it does
//! make every loggable field enumerable by reading one enum.
//!
//! The standing rules this file must not break:
//!   * no password, master password, or connection string ever — the vault
//!     owns those and they never leave it;
//!   * no result data — not a cell, not a row, not a column value;
//!   * no SQL text. History already stores SQL deliberately, in its own
//!     store, with its own retention. A second copy in a plain text file is
//!     a second thing to protect, and a diagnostic log does not need it:
//!     the statement KIND and the outcome are what tell you what happened.
//!
//! Driver error messages are logged. They can quote a fragment of a
//! statement, which is a deliberate exception: the same text is already on
//! screen, and an error you cannot read is an error you cannot fix.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotate at 2 MiB and keep exactly one older generation, so a long-running
/// session cannot fill a disk and a crash loop cannot push out the entry
/// that explains the first crash.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// A single field value is truncated to this many characters. A driver
/// error can be a page long; the log is meant to stay readable.
const MAX_FIELD: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

/// Everything the app is allowed to write to the log. See the module docs
/// before adding a variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Startup { version: &'static str },
    /// A connection attempt finished. `engine` is the driver name, never
    /// the connection string.
    ConnectOk { conn: String, engine: String, ms: u64 },
    ConnectFailed { conn: String, engine: String, error: String },
    SchemaLoaded { conn: String, db: Option<String>, tables: usize, ms: u64 },
    /// The tree was painted from the on-disk cache before the server
    /// answered. Without this entry the log cannot tell a cache HIT from a
    /// miss — both look like a later `schema.ok` — which made the one
    /// question the cache exists to answer unanswerable (2026-08-31).
    SchemaFromCache { conn: String, db: Option<String>, tables: usize, ms: u64 },
    SchemaFailed { conn: String, db: Option<String>, error: String },
    /// `kind` is the statement kind (SELECT, INSERT, …), never the text.
    QueryOk { kind: String, rows: usize, ms: u64 },
    QueryFailed { kind: String, error: String },
    /// A write that the user confirmed in the Apply dialog actually ran.
    /// `target` is the object name; the statement itself is not logged.
    WriteApplied { kind: String, target: String, affected: u64 },
    /// A context-menu or tree action was performed. This is the entry that
    /// answers „did my click arrive?".
    Action { action: String, target: String },
    /// The app declined to do something and said so only in the status bar.
    /// Every silent-looking refusal should emit one of these.
    Refused { what: String, reason: String },
    /// Persisting settings failed — otherwise invisible until a restart
    /// loses the change.
    ConfigSaveFailed { error: String },
    Panicked { location: String, message: String },
}

impl Event {
    pub fn level(&self) -> Level {
        match self {
            Event::Startup { .. }
            | Event::ConnectOk { .. }
            | Event::SchemaLoaded { .. }
            | Event::SchemaFromCache { .. }
            | Event::QueryOk { .. }
            | Event::WriteApplied { .. }
            | Event::Action { .. } => Level::Info,
            Event::Refused { .. } => Level::Warn,
            Event::ConnectFailed { .. }
            | Event::SchemaFailed { .. }
            | Event::QueryFailed { .. }
            | Event::ConfigSaveFailed { .. }
            | Event::Panicked { .. } => Level::Error,
        }
    }

    /// `key field=value …` — the part after the timestamp and level.
    fn render(&self) -> String {
        let mut s = String::new();
        match self {
            Event::Startup { version } => {
                let _ = write!(s, "startup version={}", q(version));
            }
            Event::ConnectOk { conn, engine, ms } => {
                let _ = write!(s, "connect.ok conn={} engine={} ms={ms}", q(conn), q(engine));
            }
            Event::ConnectFailed { conn, engine, error } => {
                let _ = write!(
                    s,
                    "connect.failed conn={} engine={} error={}",
                    q(conn),
                    q(engine),
                    q(error)
                );
            }
            Event::SchemaLoaded { conn, db, tables, ms } => {
                let _ = write!(
                    s,
                    "schema.ok conn={} db={} tables={tables} ms={ms}",
                    q(conn),
                    q(db.as_deref().unwrap_or("-"))
                );
            }
            Event::SchemaFromCache { conn, db, tables, ms } => {
                let _ = write!(
                    s,
                    "schema.cache conn={} db={} tables={tables} ms={ms}",
                    q(conn),
                    q(db.as_deref().unwrap_or("-"))
                );
            }
            Event::SchemaFailed { conn, db, error } => {
                let _ = write!(
                    s,
                    "schema.failed conn={} db={} error={}",
                    q(conn),
                    q(db.as_deref().unwrap_or("-")),
                    q(error)
                );
            }
            Event::QueryOk { kind, rows, ms } => {
                let _ = write!(s, "query.ok kind={} rows={rows} ms={ms}", q(kind));
            }
            Event::QueryFailed { kind, error } => {
                let _ = write!(s, "query.failed kind={} error={}", q(kind), q(error));
            }
            Event::WriteApplied { kind, target, affected } => {
                let _ = write!(
                    s,
                    "write.applied kind={} target={} affected={affected}",
                    q(kind),
                    q(target)
                );
            }
            Event::Action { action, target } => {
                // A few actions genuinely act on nothing (refresh, toggle).
                // `-` says that; `""` looks like a bug.
                let t = if target.is_empty() { "-" } else { target.as_str() };
                let _ = write!(s, "action {} target={}", q(action), q(t));
            }
            Event::Refused { what, reason } => {
                let _ = write!(s, "refused what={} reason={}", q(what), q(reason));
            }
            Event::ConfigSaveFailed { error } => {
                let _ = write!(s, "config.save.failed error={}", q(error));
            }
            Event::Panicked { location, message } => {
                let _ = write!(s, "panic at={} message={}", q(location), q(message));
            }
        }
        s
    }
}

/// Quote a field value so that no value can forge a log line.
///
/// A connection can be named anything, including a name containing a
/// newline and a plausible-looking timestamp. Without this, naming one
/// would let it write arbitrary entries.
fn q(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for (i, ch) in v.chars().enumerate() {
        if i == MAX_FIELD {
            out.push('…');
            break;
        }
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------
// Timestamps. Self-contained rather than a date dependency, matching
// `dbc-driver-duckdb`'s reasoning for the same function.
// ---------------------------------------------------------------------

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// UTC, not local time: a log read on a different machine, or across a DST
/// step, must still order correctly. The `Z` says so on every line.
fn format_ts(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn format_line(unix_secs: u64, ev: &Event) -> String {
    format!("{} {} {}", format_ts(unix_secs), ev.level().as_str(), ev.render())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// The sink.
// ---------------------------------------------------------------------

/// Owns one log file and its single older generation.
struct Sink {
    path: PathBuf,
    max_bytes: u64,
}

impl Sink {
    fn rotated(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".1");
        PathBuf::from(p)
    }

    /// Append one line. Every failure is swallowed: a database client that
    /// cannot write its log must still run.
    fn append(&self, line: &str) {
        if std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) >= self.max_bytes {
            let _ = std::fs::rename(&self.path, self.rotated());
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The last `bytes` bytes of the current file, for the in-app viewer.
    /// Starts at the first line boundary so the view never opens mid-line.
    fn tail(&self, bytes: usize) -> String {
        let Ok(data) = std::fs::read(&self.path) else {
            return String::new();
        };
        let start = data.len().saturating_sub(bytes);
        let s = String::from_utf8_lossy(&data[start..]).into_owned();
        if start == 0 {
            return s;
        }
        match s.find('\n') {
            Some(i) => s[i + 1..].to_string(),
            None => s,
        }
    }
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Point the log at `dir` (the profile directory). Before this runs — and
/// in every test that does not call it — [`log`] is a no-op, so the test
/// suite never writes a log file.
pub fn init(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = SINK.set(Mutex::new(Sink { path: dir.join("dbc.log"), max_bytes: MAX_BYTES }));
}

pub fn log(ev: Event) {
    if let Some(sink) = SINK.get() {
        let line = format_line(now_secs(), &ev);
        if let Ok(sink) = sink.lock() {
            sink.append(&line);
        }
    }
}

/// Where the log lives, once [`init`] has run.
pub fn path() -> Option<PathBuf> {
    SINK.get()?.lock().ok().map(|s| s.path.clone())
}

/// The tail of the log, for showing it inside the app.
pub fn tail(bytes: usize) -> String {
    match SINK.get().and_then(|s| s.lock().ok()) {
        Some(s) => s.tail(bytes),
        None => String::new(),
    }
}

/// Record panics before the window disappears.
///
/// A GPUI app that panics simply vanishes; without this the log's last line
/// is whatever happened to be written before, which is exactly the case the
/// log exists for. Chains to the previous hook so the normal panic message
/// still reaches stderr.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => info
                .payload()
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "<non-string panic payload>".to_string()),
        };
        let location = info.location().map(|l| l.to_string()).unwrap_or_else(|| "?".to_string());
        log(Event::Panicked { location, message });
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
    }

    #[test]
    fn a_line_starts_with_a_sortable_utc_timestamp_and_a_level() {
        let line = format_line(1_756_425_600, &Event::Startup { version: "0.22.0" });
        assert!(line.starts_with("2025-08-29 00:00:00Z INFO  startup"), "{line}");
    }

    /// The reason field values are quoted at all: a connection may be named
    /// anything the user typed.
    #[test]
    fn a_value_cannot_forge_a_log_line() {
        let evil = "prod\n2026-01-01 00:00:00Z ERROR everything is on fire";
        let line = format_line(0, &Event::Action { action: "x".into(), target: evil.into() });
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(line.contains("\\n"), "{line}");
    }

    /// An action with no object acts on nothing — say so, rather than
    /// printing an empty pair that reads like a missing value.
    #[test]
    fn an_action_without_a_target_says_so() {
        let line = format_line(0, &Event::Action { action: "Refresh".into(), target: String::new() });
        assert!(line.ends_with("target=\"-\""), "{line}");
    }

    #[test]
    fn a_very_long_value_is_truncated_rather_than_filling_the_file() {
        let long = "x".repeat(10_000);
        let line = format_line(0, &Event::QueryFailed { kind: "SELECT".into(), error: long });
        assert!(line.chars().count() < MAX_FIELD + 100, "{} chars", line.chars().count());
        assert!(line.contains('…'));
    }

    /// Levels are what a reader scans for; the mapping is worth pinning so
    /// a new variant cannot land as INFO by accident.
    #[test]
    fn failures_are_errors_and_refusals_are_warnings() {
        assert_eq!(Event::Refused { what: "a".into(), reason: "b".into() }.level(), Level::Warn);
        assert_eq!(
            Event::QueryFailed { kind: "a".into(), error: "b".into() }.level(),
            Level::Error
        );
        assert_eq!(Event::Startup { version: "v" }.level(), Level::Info);
    }

    #[test]
    fn the_file_rotates_once_and_keeps_the_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Sink { path: dir.path().join("dbc.log"), max_bytes: 200 };
        for i in 0..40 {
            sink.append(&format!("line {i} ....................."));
        }
        assert!(sink.rotated().exists(), "no rotated generation");
        assert!(
            std::fs::metadata(&sink.path).unwrap().len() < 400,
            "the live file kept growing past the cap"
        );
    }

    #[test]
    fn tail_never_starts_mid_line() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Sink { path: dir.path().join("dbc.log"), max_bytes: MAX_BYTES };
        sink.append("first line is quite long indeed");
        sink.append("second");
        sink.append("third");
        let t = sink.tail(20);
        for line in t.lines() {
            assert!(["first line is quite long indeed", "second", "third"].contains(&line), "{t}");
        }
    }

    /// The suite must not litter: without `init`, logging does nothing.
    #[test]
    fn logging_before_init_is_a_no_op() {
        log(Event::Startup { version: "test" });
    }
}
