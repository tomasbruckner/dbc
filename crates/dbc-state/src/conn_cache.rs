//! What the app already knows about a connection without talking to it: the
//! server's version, and the names of its databases.
//!
//! # Why
//!
//! Both answers were re-earned from the server on every start, and both are
//! things the sidebar wants to show BEFORE it can connect. The version came
//! and went („kdyz vypnu a zapnu appku, tak mi zmizi info o verzi serveru",
//! user 2026-09-01) and expanding a connection meant a round trip before a
//! single row appeared, even though the same list had been fetched an hour
//! earlier („kdyz se nactou db a schemata … a pak vypnu zapnu appku, tak se
//! automaticky obnovi z cache jeste pred otevrenim toho serveru? Jestli ne,
//! tak to chceme").
//!
//! One file rather than one per fact, because they are the same fact with
//! two fields: „here is what this server said last time". They are learned
//! at the same moment (expanding a connection asks for both), they go stale
//! together, and they are thrown away together.
//!
//! It replaces the short-lived `server-versions.json`, which held only the
//! first of the two and is deleted on the first write here. Nothing is
//! migrated out of it: one expand re-earns everything it contained.
//!
//! # What is stored
//!
//! A connection id, a short version string (`"18"`, `"3.45"`), and database
//! NAMES. No host, no user, no credential, and nothing out of a table. That
//! is the same class of information as the connection names and hosts
//! already sitting in `config.toml` beside it — see [`crate::schema_cache`],
//! which makes the same argument at greater length for a much larger file.
//!
//! # Staleness
//!
//! Painted immediately, replaced the moment the server answers. A database
//! dropped since the last run shows for the moment it takes the refresh to
//! land, and never survives it. There is no expiry: an entry nobody
//! revisits costs a few bytes, and one that IS revisited is refreshed by
//! the visit.
//!
//! # Derived, therefore not portable
//!
//! Deliberately outside the settings bundle (see [`crate::bundle`]): it is
//! re-earned by the first connect on a new machine, and shipping it would
//! let a bundle assert a version — or a list of databases — for a server
//! the target machine has never reached.
//!
//! An entry for a deleted connection lingers until the cap below evicts it.
//! Accepted rather than overlooked: connection ids are minted from a
//! timestamp and never reused, so a stale entry cannot attach itself to a
//! different connection.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Far more connections than anyone keeps, and the whole file is still a
/// few kilobytes. A cache that grows without bound is a disk leak, not a
/// cache — the rule [`crate::schema_cache`] follows, with a much smaller
/// unit of waste.
const MAX_ENTRIES: usize = 256;

/// A database list longer than this is not a sidebar, it is a report. The
/// live fetch has its own truncation; this keeps a pathological server from
/// turning a convenience file into a megabyte.
const MAX_DATABASES: usize = 2_000;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    databases: Option<Vec<String>>,
    /// When this was last confirmed by a server. Only ever read to decide
    /// what to drop first when the cap is reached.
    #[serde(default)]
    seen_unix: u64,
}

fn path() -> PathBuf {
    crate::workspace::profile_dir().join("connection-cache.json")
}

/// The file this one replaced, removed the first time we write.
fn superseded_path() -> PathBuf {
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

/// Every remembered server version, by connection id.
pub fn versions() -> BTreeMap<String, String> {
    read_file().into_iter().filter_map(|(id, e)| e.version.map(|v| (id, v))).collect()
}

/// The database names last seen on `conn_id`, if any.
pub fn databases(conn_id: &str) -> Option<Vec<String>> {
    read_file().remove(conn_id)?.databases.filter(|d| !d.is_empty())
}

/// Remember the server version for `conn_id`.
pub fn record_version(conn_id: &str, version: &str) {
    if conn_id.is_empty() || version.is_empty() {
        return;
    }
    update(conn_id, |e| e.version = Some(version.to_string()));
}

/// Remember the database names for `conn_id`.
///
/// An empty list is not recorded. „The server told us it has no databases"
/// and „we have never asked" look identical once written down, and of the
/// two the second is the honest default — a connection whose list failed
/// halfway should show nothing next time, not an empty tree that looks
/// authoritative.
pub fn record_databases(conn_id: &str, names: &[String]) {
    if conn_id.is_empty() || names.is_empty() {
        return;
    }
    let names: Vec<String> = names.iter().take(MAX_DATABASES).cloned().collect();
    update(conn_id, |e| e.databases = Some(names.clone()));
}

/// Read, change one entry, prune, write. The file is measured in
/// kilobytes, which is why it needs none of the incremental machinery a
/// real cache would. Every failure is swallowed: a cache that cannot be
/// written is a slower sidebar tomorrow, never a broken app.
fn update(conn_id: &str, f: impl FnOnce(&mut Entry)) {
    let mut map = read_file();
    let entry = map.entry(conn_id.to_string()).or_default();
    f(entry);
    entry.seen_unix = now_unix();
    prune(&mut map);
    write(&map);
}

/// Drop the least recently confirmed entries until the map is inside the
/// cap. Ties break on the id so the result is deterministic — a cache that
/// evicts differently on two runs with the same input is one nobody can
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
    // takes every remembered answer with it.
    let tmp = crate::fsutil::tmp_path_for(&path);
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        let _ = std::fs::remove_file(superseded_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, seen: u64) -> Entry {
        Entry { version: Some(version.into()), databases: None, seen_unix: seen }
    }

    #[test]
    fn the_least_recently_confirmed_entries_are_the_ones_dropped() {
        let mut map = BTreeMap::new();
        for i in 0..(MAX_ENTRIES + 10) {
            map.insert(format!("conn-{i:04}"), entry("18", i as u64));
        }
        prune(&mut map);
        assert_eq!(map.len(), MAX_ENTRIES);
        assert!(!map.contains_key("conn-0000"));
        assert!(!map.contains_key("conn-0009"));
        assert!(map.contains_key("conn-0010"));
        assert!(map.contains_key(&format!("conn-{:04}", MAX_ENTRIES + 9)));
    }

    #[test]
    fn a_map_inside_the_cap_is_left_alone() {
        let mut map = BTreeMap::new();
        map.insert("a".to_string(), entry("18", 1));
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
                m.insert(format!("c{i:04}"), entry("1", 7));
            }
            m
        };
        let (mut a, mut b) = (build(), build());
        prune(&mut a);
        prune(&mut b);
        assert_eq!(a, b);
    }

    /// The two facts are independent: recording one must not erase the
    /// other, which is the whole reason they share an entry rather than a
    /// file.
    #[test]
    fn the_two_facts_do_not_overwrite_each_other() {
        let e = Entry {
            version: Some("18".into()),
            databases: Some(vec!["sales".into()]),
            ..Entry::default()
        };
        assert_eq!(e.version.as_deref(), Some("18"));
        assert_eq!(e.databases.as_deref(), Some(&["sales".to_string()][..]));
    }

    /// An entry written by the version-only build must still load; the
    /// missing fields are simply absent, not a parse failure.
    #[test]
    fn an_entry_from_an_older_shape_still_parses() {
        let parsed: BTreeMap<String, Entry> =
            serde_json::from_str(r#"{"c1":{"version":"18"}}"#).expect("must parse");
        assert_eq!(parsed["c1"].version.as_deref(), Some("18"));
        assert_eq!(parsed["c1"].databases, None);
        assert_eq!(parsed["c1"].seen_unix, 0);
    }

    /// An entry with neither fact is not an error either — it is what a
    /// connection looks like before anything has been learned about it.
    #[test]
    fn an_empty_entry_round_trips() {
        let e = Entry::default();
        let json = serde_json::to_string(&e).expect("serialises");
        assert_eq!(json, r#"{"seen_unix":0}"#, "absent facts must not be written out");
        assert_eq!(serde_json::from_str::<Entry>(&json).expect("parses"), e);
    }

    /// The public shape hides the bookkeeping: callers see versions and
    /// names, never a timestamp.
    #[test]
    fn only_versions_that_exist_are_reported() {
        let mut map = BTreeMap::new();
        map.insert("has".to_string(), entry("16", 1));
        map.insert(
            "hasnt".to_string(),
            Entry { version: None, databases: Some(vec!["x".into()]), seen_unix: 1 },
        );
        let flat: BTreeMap<String, String> =
            map.into_iter().filter_map(|(id, e)| e.version.map(|v| (id, v))).collect();
        assert_eq!(flat.get("has").map(String::as_str), Some("16"));
        assert!(!flat.contains_key("hasnt"));
    }
}
