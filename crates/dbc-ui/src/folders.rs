//! Folder operations for the connection tree — the pure half.
//!
//! Folders used to be entirely IMPLICIT: a folder existed because some
//! connection named it in `ConnectionConfig::folder`, and vanished when the
//! last one left. That is enough to display a tree and not enough to manage
//! one — you cannot create an empty folder to put things into, which is the
//! order people actually work in.
//!
//! So `AppConfig::folders` now declares folders that exist on their own, and
//! the displayed set is the union of the declared ones and the ones the
//! connections imply. Every operation below is a pure function over
//! `(connections, declared folders)`, for the same reason `tree_menu` is
//! pure: GPUI has no headless harness here, so anything decided inside a
//! `render` can only ever be checked by hand.
//!
//! # The rule that matters
//!
//! **Deleting a folder never deletes a connection.** A folder is a label,
//! not a container; its connections move up to the parent. Losing a saved
//! connection — host, port, user, and the vault entry keyed to it — because
//! you tidied up a tree would be an unforgivable way to lose work.

use dbc_state::ConnectionConfig;

/// The folders to display: declared ∪ implied, with every ancestor filled
/// in, sorted parent-before-child.
///
/// Ancestors are materialised because a declared `["work", "prod"]` with no
/// declared `["work"]` would otherwise render a child with no parent — the
/// tree would either drop it or grow a second root.
pub fn visible_folders(conns: &[ConnectionConfig], declared: &[Vec<String>]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut push_with_ancestors = |path: &[String]| {
        for depth in 1..=path.len() {
            let prefix = path[..depth].to_vec();
            if !out.contains(&prefix) {
                out.push(prefix);
            }
        }
    };
    for c in conns.iter().filter(|c| !c.favourite) {
        push_with_ancestors(&c.folder);
    }
    for d in declared {
        push_with_ancestors(d);
    }
    // `Vec<String>: Ord` is lexicographic, so this is parent-before-child
    // and alphabetical within siblings in one sort — the same property
    // `group_connections` already relies on.
    out.sort();
    out
}

/// A folder path as one displayable string. The empty path is the root,
/// which has no name of its own.
pub fn label(path: &[String]) -> String {
    if path.is_empty() { "kořen".to_string() } else { path.join(" / ") }
}

/// Is `path` inside `root` (or equal to it)?
pub fn is_under(path: &[String], root: &[String]) -> bool {
    path.len() >= root.len() && path[..root.len()] == *root
}

#[derive(Debug, PartialEq, Eq)]
pub enum FolderError {
    EmptyName,
    AlreadyExists,
}

impl FolderError {
    pub fn message(&self) -> &'static str {
        match self {
            FolderError::EmptyName => "Název složky nesmí být prázdný.",
            FolderError::AlreadyExists => "Složka s tímto názvem už tady je.",
        }
    }
}

/// Add `name` under `parent`. `parent` empty = a new root folder.
pub fn create(
    conns: &[ConnectionConfig],
    declared: &[Vec<String>],
    parent: &[String],
    name: &str,
) -> Result<Vec<Vec<String>>, FolderError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(FolderError::EmptyName);
    }
    let mut path = parent.to_vec();
    path.push(name.to_string());
    if visible_folders(conns, declared).contains(&path) {
        return Err(FolderError::AlreadyExists);
    }
    let mut out = declared.to_vec();
    out.push(path);
    out.sort();
    Ok(out)
}

/// Rename the LAST segment of `path` to `name`, carrying every descendant
/// folder and every connection inside it along.
pub fn rename(
    conns: &[ConnectionConfig],
    declared: &[Vec<String>],
    path: &[String],
    name: &str,
) -> Result<(Vec<ConnectionConfig>, Vec<Vec<String>>), FolderError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(FolderError::EmptyName);
    }
    if path.is_empty() {
        return Err(FolderError::EmptyName);
    }
    let mut target = path.to_vec();
    *target.last_mut().expect("checked non-empty") = name.to_string();
    if target == path {
        // A no-op rename is not an error; nothing moves.
        return Ok((conns.to_vec(), declared.to_vec()));
    }
    if visible_folders(conns, declared).contains(&target) {
        return Err(FolderError::AlreadyExists);
    }

    let repoint = |p: &[String]| -> Vec<String> {
        if is_under(p, path) {
            let mut out = target.clone();
            out.extend_from_slice(&p[path.len()..]);
            out
        } else {
            p.to_vec()
        }
    };
    let conns = conns
        .iter()
        .cloned()
        .map(|mut c| {
            c.folder = repoint(&c.folder);
            c
        })
        .collect();
    let mut folders: Vec<Vec<String>> = declared.iter().map(|d| repoint(d)).collect();
    folders.sort();
    folders.dedup();
    Ok((conns, folders))
}

/// Remove the folder. Its connections — and the connections of every folder
/// under it — move to `path`'s PARENT. Nothing is deleted but the label.
pub fn delete(
    conns: &[ConnectionConfig],
    declared: &[Vec<String>],
    path: &[String],
) -> (Vec<ConnectionConfig>, Vec<Vec<String>>, usize) {
    if path.is_empty() {
        return (conns.to_vec(), declared.to_vec(), 0);
    }
    let parent = path[..path.len() - 1].to_vec();
    let mut moved = 0usize;
    let conns = conns
        .iter()
        .cloned()
        .map(|mut c| {
            if is_under(&c.folder, path) {
                c.folder = parent.clone();
                moved += 1;
            }
            c
        })
        .collect();
    let folders = declared.iter().filter(|d| !is_under(d, path)).cloned().collect();
    (conns, folders, moved)
}

/// Move one connection into `folder`.
///
/// Clearing `favourite` is not a side effect, it is the point: favourites
/// render in their own group ABOVE the folders and are excluded from them,
/// so dropping a starred connection into a folder without this would look
/// like the drop had been ignored.
pub fn move_connection(
    conns: &[ConnectionConfig],
    conn_id: &str,
    folder: &[String],
) -> Vec<ConnectionConfig> {
    conns
        .iter()
        .cloned()
        .map(|mut c| {
            if c.id == conn_id {
                c.folder = folder.to_vec();
                c.favourite = false;
            }
            c
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_state::Engine;

    fn conn(id: &str, folder: &[&str]) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: id.into(),
            folder: folder.iter().map(|s| s.to_string()).collect(),
            engine: Engine::Postgres,
            host: "h".into(),
            port: None,
            database: "d".into(),
            user: "u".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    fn p(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_declared_folder_shows_up_with_no_connections_in_it() {
        let got = visible_folders(&[], &[p(&["prázdná"])]);
        assert_eq!(got, vec![p(&["prázdná"])], "the whole reason folders became explicit");
    }

    #[test]
    fn a_nested_folder_materialises_its_parents() {
        let got = visible_folders(&[], &[p(&["work", "prod", "eu"])]);
        assert_eq!(got, vec![p(&["work"]), p(&["work", "prod"]), p(&["work", "prod", "eu"])]);
    }

    #[test]
    fn declared_and_implied_folders_are_unioned_without_duplicates() {
        let conns = vec![conn("a", &["work"])];
        let got = visible_folders(&conns, &[p(&["work"]), p(&["osobní"])]);
        assert_eq!(got, vec![p(&["osobní"]), p(&["work"])]);
    }

    #[test]
    fn create_refuses_an_empty_name_and_a_duplicate() {
        let declared = vec![p(&["work"])];
        assert_eq!(create(&[], &declared, &[], "   "), Err(FolderError::EmptyName));
        assert_eq!(create(&[], &declared, &[], "work"), Err(FolderError::AlreadyExists));
        assert_eq!(create(&[], &declared, &[], " nová ").unwrap().len(), 2, "trimmed and added");
    }

    #[test]
    fn create_under_a_parent_makes_a_child() {
        let out = create(&[], &[p(&["work"])], &p(&["work"]), "prod").unwrap();
        assert!(out.contains(&p(&["work", "prod"])));
    }

    #[test]
    fn rename_carries_descendants_and_connections() {
        let conns = vec![conn("a", &["work"]), conn("b", &["work", "prod"]), conn("c", &["jiné"])];
        let declared = vec![p(&["work"]), p(&["work", "prod"])];
        let (conns, folders) = rename(&conns, &declared, &p(&["work"]), "práce").unwrap();
        assert_eq!(conns[0].folder, p(&["práce"]));
        assert_eq!(conns[1].folder, p(&["práce", "prod"]), "descendant re-parented");
        assert_eq!(conns[2].folder, p(&["jiné"]), "unrelated folder untouched");
        assert_eq!(folders, vec![p(&["práce"]), p(&["práce", "prod"])]);
    }

    #[test]
    fn rename_onto_an_existing_sibling_is_refused() {
        let declared = vec![p(&["a"]), p(&["b"])];
        assert_eq!(rename(&[], &declared, &p(&["a"]), "b"), Err(FolderError::AlreadyExists));
    }

    /// THE rule: a folder is a label, not a container.
    #[test]
    fn delete_moves_connections_up_and_destroys_none() {
        let conns = vec![
            conn("a", &["work"]),
            conn("b", &["work", "prod"]),
            conn("c", &["jiné"]),
        ];
        let declared = vec![p(&["work"]), p(&["work", "prod"])];
        let (out, folders, moved) = delete(&conns, &declared, &p(&["work"]));
        assert_eq!(out.len(), 3, "a connection was destroyed");
        assert_eq!(moved, 2);
        assert_eq!(out[0].folder, Vec::<String>::new(), "moved to the root");
        assert_eq!(out[1].folder, Vec::<String>::new(), "nested one came up too");
        assert_eq!(out[2].folder, p(&["jiné"]));
        assert!(folders.is_empty(), "the folder and its child are gone");
    }

    #[test]
    fn deleting_a_nested_folder_moves_to_its_parent_not_the_root() {
        let conns = vec![conn("a", &["work", "prod"])];
        let (out, _, _) = delete(&conns, &[], &p(&["work", "prod"]));
        assert_eq!(out[0].folder, p(&["work"]));
    }

    /// Dropping a starred connection into a folder has to visibly move it.
    #[test]
    fn moving_a_favourite_into_a_folder_unstars_it() {
        let mut c = conn("a", &[]);
        c.favourite = true;
        let out = move_connection(&[c], "a", &p(&["work"]));
        assert_eq!(out[0].folder, p(&["work"]));
        assert!(!out[0].favourite, "it would have stayed in the favourites group");
    }

    #[test]
    fn moving_touches_only_the_named_connection() {
        let conns = vec![conn("a", &["x"]), conn("b", &["y"])];
        let out = move_connection(&conns, "a", &p(&["z"]));
        assert_eq!(out[0].folder, p(&["z"]));
        assert_eq!(out[1].folder, p(&["y"]));
    }

    #[test]
    fn is_under_is_prefix_not_substring() {
        assert!(is_under(&p(&["work", "prod"]), &p(&["work"])));
        assert!(is_under(&p(&["work"]), &p(&["work"])));
        assert!(!is_under(&p(&["workshop"]), &p(&["work"])), "segment-wise, not textual");
        assert!(!is_under(&p(&["work"]), &p(&["work", "prod"])));
    }
}
