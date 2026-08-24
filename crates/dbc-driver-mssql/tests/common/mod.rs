//! Shared docker/env plumbing for the MSSQL live tier (G15 T8). Used by
//! `mssql_integration.rs` and `mssql_tx_matrix.rs` via `mod common;`.
//! `DBC_MSSQL_TEST_CONN` stays the escape hatch (existing convention,
//! useful for pointing at an already-running container while iterating);
//! testcontainers is the default when unset. The container is started ONCE
//! per test process and deliberately leaked (`std::mem::forget`) — startup
//! is 30-60s and the image ~1.5GB, so per-test containers (the pg
//! precedent) are not viable here.
//!
//! CORRECTION (whole-branch review M2 finding): the claim that
//! "testcontainers' reaper (ryuk) removes it after the process exits" was
//! WRONG on this host — ryuk does not reap these containers here (Docker
//! Desktop/Windows networking, not investigated further), so EVERY
//! `cargo test -- --ignored` run that starts a fresh container (i.e. every
//! run without `DBC_MSSQL_TEST_CONN` set) leaks a ~1GB SQL Server
//! container that survives the test process. `docker ps`/`docker rm -f`
//! it manually after a live run. For repeated local iteration, set
//! `DBC_MSSQL_TEST_CONN` to point at ONE manually-started, long-lived
//! container instead of leaking a new one per invocation — this is the
//! cheap mitigation this comment recommends in lieu of a real fix (a real
//! fix would mean either getting ryuk working on this host or adding
//! process-exit cleanup logic neither of which this task attempted).

use tokio::sync::OnceCell;

static CONN: OnceCell<Option<String>> = OnceCell::const_new();

pub async fn conn_str_or_skip(test: &str) -> Option<String> {
    let s = CONN
        .get_or_init(|| async {
            if let Ok(s) = std::env::var("DBC_MSSQL_TEST_CONN") {
                return Some(s);
            }
            use testcontainers_modules::{
                mssql_server::MssqlServer, testcontainers::runners::AsyncRunner,
            };
            // ACCEPT_EULA is NOT set by Default — the explicit call is
            // required or the container exits immediately (verified
            // against the vendored 0.13.0 module source).
            let container = MssqlServer::default().with_accept_eula().start().await.ok()?;
            let host = container.get_host().await.ok()?;
            let port = container.get_host_port_ipv4(1433).await.ok()?;
            std::mem::forget(container);
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
