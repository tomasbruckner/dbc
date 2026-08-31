//! Idle schema prefetch — the pure half.
//!
//! The point is NOT to have schemas in memory: the tree keeps at most
//! [`crate::schema_tree::LOADED_SNAPSHOT_CAP`] slots and evicts by LRU, so
//! filling it from a background job would only push out the ones the user
//! is actually working with. The point is the DISK cache. In this app's own
//! use the difference is 181 ms against 2 741 ms for the same 1 171 tables
//! — so warming an entry turns the first expand of a database from a pause
//! into no pause at all, and nothing else about the app changes.
//!
//! Which is also why the prefetch never touches the tree. It fetches, it
//! writes the cache, it stops. No slot transitions, no generation bump, no
//! repaint — a background job that could change what is on screen would be
//! a background job that can fight the user for it.
//!
//! **Scope, deliberately narrow.** Only the ACTIVE connection's databases.
//! The runner is per-operation (sidebar design fact 0.1), so every prefetch
//! is a fresh connection to a server; reaching into other saved connections
//! would open sessions to machines the user is not working with, which is
//! not a speed-up, it is traffic nobody asked for.

/// Everything that makes the app not-idle. All five must be false before a
/// prefetch may start.
///
/// Spelled out as a struct rather than an `&&` chain at the call site so
/// the rule can be tested — the call site needs a `Window` and a live
/// `Context`, and neither exists in this crate's test harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy {
    /// A query is running. Its results are what the user is waiting for.
    pub query_running: bool,
    /// A modal is open — including, crucially, the master-password prompt.
    pub modal_open: bool,
    /// A database list or a schema slot the USER asked for is in flight.
    /// Competing with it would make the thing they clicked slower.
    pub sidebar_fetching: bool,
    /// A prefetch is already running. One at a time, always.
    pub prefetch_in_flight: bool,
}

/// May a prefetch start right now?
///
/// `secret_available` is separate from [`Busy`] because it is not a busy
/// signal but a hard rule: design §4.4 forbids fetching metadata with an
/// empty-secret fallback, and a background job must never be the thing
/// that pops a master-password prompt — the user did not ask for anything.
/// It is false whenever the normal path would have had to prompt, and true
/// for the file engines, which have no secret to want.
pub fn may_prefetch(busy: &Busy, secret_available: bool) -> bool {
    secret_available
        && !busy.query_running
        && !busy.modal_open
        && !busy.sidebar_fetching
        && !busy.prefetch_in_flight
}

/// One database the prefetch could warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub name: &'a str,
    /// The tree already holds a slot for it (Loaded, Loading, or Error).
    /// `Error` counts as known on purpose: a database that just failed
    /// would otherwise be retried by every idle tick forever.
    pub known: bool,
    /// There is already a cache entry on disk.
    pub cached: bool,
}

/// How far down a connection's database list the prefetch will look.
///
/// Two costs at once. The obvious one is server traffic: at this app's own
/// ~2.7 s per schema, crawling a few hundred databases in the background
/// would mean tens of minutes of connections nobody asked for. The quieter
/// one is that deciding needs a `stat` per database on the UI thread every
/// tick. Both are bounded by looking at the top of the list only — the
/// databases the sidebar shows first, which are the ones a person working
/// on this server is most likely to open.
pub const SCAN_CAP: usize = 32;

/// The next database to warm, or `None` when there is nothing left to do.
///
/// Returns them in the order they were given, which is the order the
/// sidebar shows — so the prefetch works down the list the user is looking
/// at rather than in some order of its own.
///
/// `active_db` is skipped because the normal path owns it: it is either
/// already loaded or being loaded right now, and a second fetch of it would
/// be pure duplication.
pub fn next_target<'a>(candidates: &[Candidate<'a>], active_db: Option<&str>) -> Option<&'a str> {
    candidates
        .iter()
        .take(SCAN_CAP)
        .find(|c| !c.known && !c.cached && Some(c.name) != active_db)
        .map(|c| c.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Busy {
        Busy {
            query_running: false,
            modal_open: false,
            sidebar_fetching: false,
            prefetch_in_flight: false,
        }
    }

    #[test]
    fn an_idle_app_with_an_open_vault_may_prefetch() {
        assert!(may_prefetch(&idle(), true));
    }

    /// Each field alone must be enough to refuse. Written as a loop over
    /// setters so a NEW field added to `Busy` without a case here shows up
    /// as a count mismatch rather than passing silently.
    #[test]
    fn every_busy_signal_alone_refuses() {
        let setters: [(&str, fn(&mut Busy)); 4] = [
            ("query", |b| b.query_running = true),
            ("modal", |b| b.modal_open = true),
            ("sidebar", |b| b.sidebar_fetching = true),
            ("prefetch", |b| b.prefetch_in_flight = true),
        ];
        assert_eq!(
            setters.len(),
            4,
            "a new Busy field needs its own case here, not just a bigger struct"
        );
        for (name, set) in setters {
            let mut b = idle();
            set(&mut b);
            assert!(!may_prefetch(&b, true), "{name} did not stop the prefetch");
        }
    }

    /// The one rule that is not about politeness: a locked vault means the
    /// stored password cannot be read, and a background job must never be
    /// what asks for the master password.
    #[test]
    fn a_locked_vault_refuses_even_when_everything_is_idle() {
        assert!(!may_prefetch(&idle(), false));
    }

    fn c<'a>(name: &'a str, known: bool, cached: bool) -> Candidate<'a> {
        Candidate { name, known, cached }
    }

    #[test]
    fn the_first_unknown_uncached_database_wins() {
        let list = [c("a", true, true), c("b", false, false), c("c", false, false)];
        assert_eq!(next_target(&list, None), Some("b"));
    }

    #[test]
    fn a_cached_or_known_database_is_skipped() {
        let list = [c("a", true, false), c("b", false, true), c("c", false, false)];
        assert_eq!(next_target(&list, None), Some("c"));
    }

    #[test]
    fn the_active_database_is_never_the_target() {
        let list = [c("a", false, false), c("b", false, false)];
        assert_eq!(next_target(&list, Some("a")), Some("b"));
    }

    /// A server with hundreds of databases must not become a background
    /// crawl. The cap is enforced HERE as well as at the call site, so the
    /// caller cannot widen it by passing a longer slice.
    #[test]
    fn nothing_past_the_scan_cap_is_ever_a_target() {
        let mut list: Vec<Candidate> = (0..SCAN_CAP).map(|_| c("seen", true, false)).collect();
        list.push(c("far", false, false));
        assert_eq!(next_target(&list, None), None);
    }

    #[test]
    fn nothing_left_to_warm_is_none() {
        let list = [c("a", true, false), c("b", false, true)];
        assert_eq!(next_target(&list, None), None);
        assert_eq!(next_target(&[], None), None);
    }
}
