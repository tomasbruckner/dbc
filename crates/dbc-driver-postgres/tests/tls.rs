//! TLS against a real server. Docker required. Run with:
//! cargo test -p dbc-driver-postgres --test tls -- --ignored
//!
//! The server is the Debian `postgres:17` image with `ssl=on` on its own
//! snakeoil (self-signed) certificate, and a `pg_hba.conf` rewritten to
//! `hostssl` only — the exact shape that refused plaintext dbc with
//! „no pg_hba.conf entry … no encryption".
use dbc_core::{CancelToken, Connection};
use dbc_driver_postgres::{PgConfig, PgSsl, PostgresConnection};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

async fn start_ssl_only_server() -> (ContainerAsync<Postgres>, u16) {
    let node = Postgres::default()
        .with_name("postgres")
        .with_tag("17")
        .with_cmd([
            "-c",
            "ssl=on",
            "-c",
            "ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem",
            "-c",
            "ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key",
        ])
        .start()
        .await
        .unwrap();
    let port = node.get_host_port_ipv4(5432).await.unwrap();

    // Still plaintext-allowed at this point: lock the server down to
    // hostssl, then reload. Server-side COPY is the only way to write the
    // file over SQL; the superuser may.
    let mut admin = PostgresConnection::connect_with_config(config(port), PgSsl::Disable)
        .await
        .unwrap();
    admin
        .execute(
            "DO $$ BEGIN EXECUTE format(\
               'COPY (SELECT %L UNION ALL SELECT %L) TO %L', \
               'local all all trust', \
               'hostssl all all all scram-sha-256', \
               current_setting('hba_file')); END $$",
            CancelToken::new(),
        )
        .await
        .unwrap();
    admin.execute("SELECT pg_reload_conf()", CancelToken::new()).await.unwrap();
    // pg_reload_conf only signals the postmaster; give it a moment.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    (node, port)
}

fn config(port: u16) -> PgConfig {
    let mut c = PgConfig::new();
    c.host("127.0.0.1").port(port).user("postgres").password("postgres").dbname("postgres");
    c
}

async fn session_is_encrypted(conn: &mut PostgresConnection) -> String {
    let mut s = conn
        .query("SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()", CancelToken::new())
        .await
        .unwrap();
    let b = s.batches.recv().await.unwrap().unwrap();
    let col = b.column(0).as_any().downcast_ref::<dbc_core::arrow::array::StringArray>().unwrap();
    col.value(0).to_string()
}

#[tokio::test]
#[ignore]
async fn hostssl_only_server_refuses_plaintext_and_accepts_prefer_and_require() {
    let (_node, port) = start_ssl_only_server().await;

    let err = PostgresConnection::connect_with_config(config(port), PgSsl::Disable)
        .await
        .err()
        .expect("plaintext must be refused by a hostssl-only pg_hba");
    assert!(err.message.contains("no encryption"), "{}", err.message);

    for ssl in [PgSsl::Prefer, PgSsl::Require] {
        let mut c = PostgresConnection::connect_with_config(config(port), ssl).await.unwrap();
        assert_eq!(session_is_encrypted(&mut c).await, "true", "{ssl:?}");
    }
}

/// The cancel request is a second connection; on a hostssl-only server it
/// is refused unless it, too, goes over TLS.
#[tokio::test]
#[ignore]
async fn cancel_travels_over_tls_too() {
    let (_node, port) = start_ssl_only_server().await;
    let mut c = PostgresConnection::connect_with_config(config(port), PgSsl::Require).await.unwrap();
    let cancel = CancelToken::new();
    let mut s = c.query("SELECT pg_sleep(30)", cancel.clone()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    cancel.cancel();
    let mut cancelled = false;
    while let Some(r) = s.batches.recv().await {
        if let Err(e) = r {
            cancelled = e.code.as_deref() == Some("cancelled");
        }
    }
    assert!(cancelled, "query was not cancelled server-side");
    assert!(t.elapsed().as_secs() < 5, "cancel took {:?}", t.elapsed());
}

/// A self-signed certificate is NOT in the Windows store, so verify-full
/// must refuse it — and must not fall back to plaintext either.
#[tokio::test]
#[ignore]
async fn verify_full_refuses_a_self_signed_certificate() {
    let (_node, port) = start_ssl_only_server().await;
    let err = PostgresConnection::connect_with_config(config(port), PgSsl::VerifyFull)
        .await
        .err()
        .expect("snakeoil certificate must not verify");
    assert!(err.message.to_lowercase().contains("certificate"), "{}", err.message);
}
