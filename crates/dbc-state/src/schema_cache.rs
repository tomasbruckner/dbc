//! On-disk cache of schema snapshots, so reopening a database you have
//! already visited is instant.
//!
//! # What is stored, and what is not
//!
//! A [`SchemaSnapshot`] is METADATA: table, view, column, routine, trigger
//! and sequence NAMES, their types, and the DDL the driver handed back for
//! routines. It contains no row data and no credentials, and this module
//! must never be handed anything else.
//!
//! The functions are generic rather than typed to `SchemaSnapshot` on
//! purpose: `dbc-state` owns the profile directory, `dbc-core` owns the
//! schema types, and neither needs to depend on the other for a cache to
//! exist. The caller names the type.
//!
//! It still lands in a plain file in the profile directory, so it is
//! readable by anything that can read the profile. That is the same
//! exposure `config.toml` (connection names, hosts, users) and
//! `history.sqlite` (every statement you have run) already carry — the
//! vault is the one file that is encrypted, because it is the one file with
//! secrets in it. A schema cache does not change that boundary; it would if
//! it ever held a row, which is why it cannot.
//!
//! # Staleness
//!
//! The cache is never trusted as the truth. The caller paints it
//! immediately and refreshes from the server in the background, replacing
//! it when the real answer arrives — so a stale entry costs one frame of
//! out-of-date names, never a wrong answer that persists. There is
//! deliberately no expiry: an entry that is never revisited costs one small
//! file, and an entry that IS revisited is refreshed by the visit itself.

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Serialize};

/// Most databases anyone keeps in a sidebar, and then some. Beyond this the
/// least recently written entry is dropped — a cache that grows without
/// bound is a disk leak, not a cache.
const MAX_ENTRIES: usize = 64;

/// …and a count alone is not a budget. One real database in this app's own
/// use serialises to 4.6 MB (1171 tables), so 64 entries could mean 300 MB
/// of cache for a convenience feature. Whichever limit bites first wins.
const MAX_BYTES: u64 = 128 * 1024 * 1024;

fn dir() -> PathBuf {
    crate::workspace::profile_dir().join("schema-cache")
}

/// FNV-1a over `conn_id` and `db`, hex — a filename that is safe on every
/// filesystem no matter what the connection is called.
///
/// A hash, not the names themselves, for two reasons: connection names and
/// database names can contain path separators, colons and every other
/// character Windows refuses in a filename; and the file names in a shared
/// profile directory would otherwise advertise which databases someone
/// works with.
fn key(conn_id: &str, db: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in conn_id.as_bytes().iter().chain(b"\x00").chain(db.as_bytes()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}.json")
}

/// The cached snapshot for `(conn_id, db)`, if there is one and it still
/// parses. A cache miss and a corrupt entry are the same answer — fetch it
/// again — so neither is an error worth reporting.
pub fn load<T: DeserializeOwned>(conn_id: &str, db: &str) -> Option<T> {
    let path = dir().join(key(conn_id, db));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the snapshot for `(conn_id, db)`. Every failure is swallowed: a
/// cache that cannot be written is a slow app, never a broken one.
pub fn store<T: Serialize>(conn_id: &str, db: &str, snapshot: &T) {
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string(snapshot) else { return };
    let path = dir.join(key(conn_id, db));
    // Same write-then-rename as every other file this app owns: a crash
    // mid-write must not leave a half-written entry that parses.
    let tmp = crate::fsutil::tmp_path_for(&path);
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    prune(&dir);
}

/// Drop the oldest entries until the cache is inside BOTH budgets.
fn prune(dir: &Path) {
    prune_to(dir, MAX_ENTRIES, MAX_BYTES);
}

fn prune_to(dir: &Path, max_entries: usize, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), e.path()))
        })
        .collect();
    files.sort_by_key(|(t, _, _)| *t);
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    let mut count = files.len();
    for (_, len, path) in files.iter() {
        if count <= max_entries && total <= max_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*len);
            count -= 1;
        }
    }
}

/// Forget everything. The settings escape hatch for „the cache is wrong",
/// and the only way to remove metadata about a database you no longer want
/// recorded.
pub fn clear() {
    let _ = std::fs::remove_dir_all(dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_a_safe_filename_whatever_the_connection_is_called() {
        for (c, d) in [
            ("c:\\weird\\name", "db"),
            ("../../etc/passwd", "db"),
            ("normal", "prod"),
            ("", ""),
        ] {
            let k = key(c, d);
            assert!(k.ends_with(".json"));
            assert!(
                k.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '.' || ch == 'j' || ch == 's'
                    || ch == 'o' || ch == 'n'),
                "{k:?} is not a plain filename"
            );
            assert!(!k.contains('/') && !k.contains('\\'));
        }
    }

    /// The pair is what identifies an entry — the same database name under
    /// two connections must not collide.
    #[test]
    fn the_key_separates_the_connection_from_the_database() {
        assert_ne!(key("a", "bc"), key("ab", "c"));
        assert_ne!(key("a", "b"), key("b", "a"));
        assert_eq!(key("a", "b"), key("a", "b"));
    }

    /// The byte budget must bite even when the entry COUNT is fine — the
    /// case that motivated it is a handful of very large schemas, not many
    /// small ones.
    #[test]
    fn prune_enforces_the_byte_budget_not_just_the_count() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(1024);
        for i in 0..4 {
            let p = tmp.path().join(format!("{i:016x}.json"));
            std::fs::write(&p, &big).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Four entries, well under MAX_ENTRIES, but over a budget this
        // small — so the count check alone would keep all four.
        prune_to(tmp.path(), MAX_ENTRIES, 2048);
        let left = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(left, 2, "byte budget did not evict");
    }

    #[test]
    fn prune_keeps_the_newest_entries_only() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            let p = tmp.path().join(format!("{i:016x}.json"));
            std::fs::write(&p, "{}").unwrap();
            // Distinct mtimes; the filesystem's resolution is coarse enough
            // that writing in a tight loop can give several files the same
            // stamp, which would make the assertion below flaky.
            filetime_bump(&p, i as u64);
        }
        prune(tmp.path());
        let left = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(left, MAX_ENTRIES);
    }

    /// `std::fs` has no set-mtime, and this crate is not adding a
    /// dependency for a test helper: re-writing the file in order is enough
    /// to give the entries increasing modification times on every platform
    /// this runs on.
    fn filetime_bump(path: &Path, _i: u64) {
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _ = std::fs::write(path, "{}");
    }
}
