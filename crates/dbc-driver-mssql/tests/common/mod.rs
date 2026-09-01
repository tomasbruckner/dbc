//! One SHARED container, reused across runs.
//!
//! `ReuseDirective::Always` plus a fixed container name means the first
//! live run starts `dbc-test-mssql` and every later run — in this crate or
//! `dbc-ui` — finds and reuses it. It is deliberately NOT reaped when the
//! test process exits.
//!
//! That is a trade made with the numbers in hand. This used to
//! `std::mem::forget` an anonymous container instead, because ryuk (the
//! testcontainers reaper) does not reap on this host, and a shared
//! container that gets dropped at the end of the first test is no use to
//! the second. The cost was one ~1.25 GB SQL Server left behind PER RUN:
//! measured 2026-08-31, a session of repeated live runs had accumulated 20
//! of them, over 20 GB of RAM, which slowed the machine enough to make a
//! timing-bound test fail and to send an investigation chasing its own
//! mess. One container that stays is a cost you can see and remove; N that
//! grow silently are not.
//!
//! **The other side of the trade: STATE NOW ACCUMULATES.** A reused
//! container keeps whatever previous runs created — databases, tables,
//! logins. Every test here already names its objects uniquely or drops
//! them, which is why this is safe today; a NEW test that assumes a
//! virgin server would pass alone and fail on the second run, and that is
//! the failure mode to suspect first if one ever does. `docker rm -f` the
//! container to get a clean one.
//!
//! Remove it when you are done with live testing:
//!
//! ```text
//! docker rm -f dbc-test-mssql
//! ```
//!
//! `DBC_MSSQL_TEST_CONN` still short-circuits all of this — point it at any
//! server of your own and no container is started at all.

use tokio::sync::OnceCell;

/// The one container both crates share. A CROSS-CRATE CONSTANT in spirit
/// — `dbc-ui`'s `mssql_docker_tests::host_port` spells the same string,
/// and they cannot import from each other. If they ever disagree the only
/// symptom is two containers instead of one, which is exactly the kind of
/// quiet waste this whole change is about.
pub const SHARED_CONTAINER_NAME: &str = "dbc-test-mssql";

static CONN: OnceCell<Option<String>> = OnceCell::const_new();

pub async fn conn_str_or_skip(test: &str) -> Option<String> {
    let s = CONN
        .get_or_init(|| async {
            if let Ok(s) = std::env::var("DBC_MSSQL_TEST_CONN") {
                return Some(s);
            }
            use testcontainers_modules::{
                mssql_server::MssqlServer,
                testcontainers::{runners::AsyncRunner, ImageExt, ReuseDirective},
            };
            // ACCEPT_EULA is NOT set by Default — the explicit call is
            // required or the container exits immediately (verified
            // against the vendored 0.13.0 module source).
            //
            // The name and the reuse directive must match `dbc-ui`'s
            // helper EXACTLY or the two crates start a container each
            // instead of sharing one — see `SHARED_CONTAINER_NAME`.
            let container = MssqlServer::default()
                .with_accept_eula()
                .with_container_name(SHARED_CONTAINER_NAME)
                .with_reuse(ReuseDirective::Always)
                .start()
                .await
                .ok()?;
            let host = container.get_host().await.ok()?;
            let port = container.get_host_port_ipv4(1433).await.ok()?;
            // No `mem::forget`: `ReuseDirective::Always` already makes Drop
            // decline to reap, so the guard can die honestly.
            // TrustServerCertificate=yes: the container's self-signed dev
            // cert — the documented dialog path (§1c), never a default.
            Some(format!(
                "Driver={{ODBC Driver 18 for SQL Server}};Server={{tcp:{host},{port}}};\
                 Database=tempdb;Uid=sa;Pwd={{yourStrong(!)Password}};\
                 Encrypt=yes;TrustServerCertificate=yes;"
            ))
        })
        .await
        .clone();
    if s.is_none() {
        eprintln!("SKIP {test}: docker unavailable and DBC_MSSQL_TEST_CONN not set");
    }
    s
}

/// Honest SKIP for the one prerequisite docker cannot provide: no host
/// ODBC Driver 17/18. IM002-probe based — dbc-ui has no odbc-api dep, and
/// a second odbc Environment would violate the one-per-process rule, so
/// BOTH crates detect via the error the missing driver actually produces.
pub fn skip_if_no_odbc_driver(test: &str, e: &dbc_core::QueryError) -> bool {
    let missing = e.code.as_deref() == Some("IM002") || e.message.contains("IM002");
    if missing {
        eprintln!(
            "SKIP {test}: ODBC Driver 18 for SQL Server není nainstalován (IM002) — \
             install msodbcsql18 to run this test live"
        );
    }
    missing
}
