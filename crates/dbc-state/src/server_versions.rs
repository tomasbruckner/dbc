//! The last server version seen on each connection, so the sidebar can say
//! „pg 18" before it has talked to anything.
//!
//! # Why this exists
//!
//! The version is read from the server when a connection is expanded, and
//! it lived only in the sidebar's in-memory node — so it appeared a second
//! after the first expand and vanished on the next start. „kdyz vypnu a
//! zapnu appku, tak mi zmizi info o verzi serveru" (user, 2026-09-01).
//! A label that comes and goes is worse than one that is simply absent,
//! because you cannot tell „this server did not answer" from „I have not
//! asked yet".
//!
//! # What is stored
//!
//! A connection id and a short version string — `"18"`, `"3.45"`. Nothing
//! else: no host, no user, no credential, and nothing that came out of a
//! database. Same exposure as the connection names already in
//! `config.toml`, in the same directory.
//!
//! # Staleness
//!
//! The same posture as [`crate::schema_cache`], and for the same reason:
//! this is painted immediately and replaced the moment the server answers
//! for real. Upgrade a server and the row is one connect out of date, never
//! permanently wrong. There is no expiry — an entry nobody revisits costs a
//! few bytes, and an entry that IS revisited is refreshed by the visit.
//!
//! # Derived, therefore not portable
//!
//! Deliberately outside the settings bundle (see [`crate::bundle`]): it is
//! re-earned by the first connect on the new machine, and shipping it would
//! mean the bundle could assert a version for a server the target machine
//! has never reached.
//!
//! An entry for a deleted connection lingers until the cap below evicts it.
//! That is accepted rather than overlooked: connection ids are minted from
//! a timestamp and never reused, so a stale entry can never attach itself
//! to a different connection, and what it holds is an id and a number.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Far more connections than anyone keeps, and the whole file is still a
/// couple of kilobytes. A cache that grows without bound is a disk leak,
/// not a cache — the same rule [`crate::schema_cache`] follows, with a much
/// smaller unit of waste.
const MAX_ENTRIES: usize = 256;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Entry {
    version: String,
    /// When this was last confirmed by a server. Only ever read to decide
    /// what to drop first when the cap is reached.
    #[serde(default)]
    seen_unix: u64,
}

fn path() -> PathBuf {
    crate::workspace::profile_dir().join("server-versions.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_file() -> BTreeMap<String, Entry> {
    let Ok(text) = std::fs::read_to_string(path()) else { return BTreeMap::new() };
    // A corrupt file is a cache miss, not an error: the next connect
    // rewrites it. Reporting it would teach the user to care about a file
    // whose whole purpose is to be disposable.
    serde_json::from_str(&text).unwrap_or_default()
}

/// Every remembered version, by connection id.
pub fn load() -> BTreeMap<String, String> {
    read_file().into_iter().map(|(id, e)| (id, e.version)).collect()
}

/// Remember `version` for `conn_id`.
///
/// Read-modify-write on a file measured in bytes, which is why it does not
/// need the incremental machinery a real cache would. Every failure is
/// swallowed — a version that cannot be written is a row that says „pg"
/// tomorrow, never a broken app.
pub fn record(conn_id: &str, version: &str) {
    if conn_id.is_empty() || version.is_empty() {
        return;
    }
    let mut map = read_file();
    map.insert(
        conn_id.to_string(),
        Entry { version: version.to_string(), seen_unix: now_unix() },
    );
    prune(&mut map);
    write(&map);
}

/// Drop the least recently confirmed entries until the map is inside the
/// cap. Ties break on the id so the result is deterministic — a cache that
/// evicts differently on two runs with the same input is a cache nobody can
/// reason about.
fn prune(map: &mut BTreeMap<String, Entry>) {
    if map.len() <= MAX_ENTRIES {
        return;
    }
    let mut by_age: Vec<(u64, String)> =
        map.iter().map(|(id, e)| (e.seen_unix, id.clone())).collect();
    by_age.sort();
    for (_, id) in by_age.into_iter().take(map.len() - MAX_ENTRIES) {
        map.remove(&id);
    }
}

fn write(map: &BTreeMap<String, Entry>) {
    let Ok(json) = serde_json::to_string_pretty(map) else { return };
    let path = path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // Write-then-rename, like every other file this app owns: a crash
    // mid-write must not leave half a JSON object that fails to parse and
    // takes every remembered version with it.
    let tmp = crate::fsutil::tmp_path_for(&path);
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eviction rule, exercised directly — the file I/O around it needs
    /// a profile directory, which these tests deliberately do not take
    /// over from whatever else is running.
    #[test]
    fn the_least_recently_confirmed_entries_are_the_ones_dropped() {
        let mut map = BTreeMap::new();
        for i in 0..(MAX_ENTRIES + 10) {
            map.insert(
                format!("conn-{i:04}"),
                Entry { version: "18".into(), seen_unix: i as u64 },
            );
        }
        prune(&mut map);
        assert_eq!(map.len(), MAX_ENTRIES);
        // The ten oldest went, the newest stayed.
        assert!(!map.contains_key("conn-0000"));
        assert!(!map.contains_key("conn-0009"));
        assert!(map.contains_key("conn-0010"));
        assert!(map.contains_key(&format!("conn-{:04}", MAX_ENTRIES + 9)));
    }

    #[test]
    fn a_map_inside_the_cap_is_left_alone() {
        let mut map = BTreeMap::new();
        map.insert("a".to_string(), Entry { version: "18".into(), seen_unix: 1 });
        let before = map.clone();
        prune(&mut map);
        assert_eq!(map, before);
    }

    /// Two runs with the same input must evict the same entries.
    #[test]
    fn eviction_is_deterministic_when_timestamps_tie() {
        let build = || {
            let mut m = BTreeMap::new();
            for i in 0..(MAX_ENTRIES + 3) {
                m.insert(format!("c{i:04}"), Entry { version: "1".into(), seen_unix: 7 });
            }
            m
        };
        let (mut a, mut b) = (build(), build());
        prune(&mut a);
        prune(&mut b);
        assert_eq!(a, b);
    }

    /// A version string is all that survives the round trip — the
    /// timestamp is bookkeeping and no caller ever sees it.
    #[test]
    fn the_public_shape_is_id_to_version() {
        let mut map = BTreeMap::new();
        map.insert("c1".to_string(), Entry { version: "16".into(), seen_unix: 99 });
        let flat: BTreeMap<String, String> =
            map.into_iter().map(|(id, e)| (id, e.version)).collect();
        assert_eq!(flat.get("c1").map(String::as_str), Some("16"));
    }

    /// An entry written by an older build has no timestamp; it must load
    /// rather than take the whole file down with it.
    #[test]
    fn an_entry_without_a_timestamp_still_parses() {
        let parsed: BTreeMap<String, Entry> =
            serde_json::from_str(r#"{"c1":{"version":"18"}}"#).expect("must parse");
        assert_eq!(parsed["c1"].version, "18");
        assert_eq!(parsed["c1"].seen_unix, 0);
    }
}
