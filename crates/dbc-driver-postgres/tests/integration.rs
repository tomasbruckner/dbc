//! Docker required. Run with: cargo test -p dbc-driver-postgres -- --ignored
use dbc_core::{CancelToken, Connection};
use dbc_driver_postgres::PostgresConnection;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

async fn pg_url(node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>) -> String {
    format!(
        "postgres://postgres:postgres@127.0.0.1:{}/postgres",
        node.get_host_port_ipv4(5432).await.unwrap()
    )
}

#[tokio::test]
#[ignore]
async fn streams_100k_rows_first_batch_early() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let started = std::time::Instant::now();
    let mut s = c
        .query("SELECT g AS id, 'row' || g AS name FROM generate_series(1, 100000) g", CancelToken::new())
        .await
        .unwrap();
    assert_eq!(s.columns.field(0).name(), "id");
    let first = s.batches.recv().await.unwrap().unwrap();
    let first_at = started.elapsed();
    let mut rows = first.num_rows();
    while let Some(b) = s.batches.recv().await { rows += b.unwrap().num_rows(); }
    assert_eq!(rows, 100_000);
    // First batch must arrive well before all 100k are done streaming.
    assert!(first_at.as_millis() < 1500, "first batch too late: {first_at:?}");
}

#[tokio::test]
#[ignore]
async fn typed_columns_come_back_typed() {
    use dbc_core::arrow::datatypes::DataType;
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let mut s = c
        .query("SELECT 1::int4 a, 2::int8 b, 3.5::float8 c, true d, 'x'::text e, 1.23::numeric f", CancelToken::new())
        .await
        .unwrap();
    let dts: Vec<DataType> = s.columns.fields().iter().map(|f| f.data_type().clone()).collect();
    assert_eq!(dts, vec![DataType::Int32, DataType::Int64, DataType::Float64, DataType::Boolean, DataType::Utf8, DataType::Utf8]);
    let b = s.batches.recv().await.unwrap().unwrap();
    assert_eq!(b.num_rows(), 1);
}

#[tokio::test]
#[ignore]
async fn cancel_kills_server_side_query() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let cancel = CancelToken::new();
    let mut s = c.query("SELECT pg_sleep(30)", cancel.clone()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    cancel.cancel();
    let mut cancelled = false;
    while let Some(r) = s.batches.recv().await {
        if let Err(e) = r { cancelled = e.code.as_deref() == Some("cancelled"); }
    }
    assert!(cancelled, "no cancelled error surfaced");
    assert!(t.elapsed().as_secs() < 5, "cancel took too long — not protocol-level");
}

#[tokio::test]
#[ignore]
async fn error_carries_sqlstate_and_position() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    // Note: dbc_core::QueryStream doesn't derive Debug, so `unwrap_err()`
    // (which requires the Ok type to be Debug) won't compile here; use
    // err().unwrap() instead, which only needs QueryError: Debug.
    let err = c.query("SELEC 1", CancelToken::new()).await.err().unwrap();
    assert_eq!(err.code.as_deref(), Some("42601"));
    assert!(err.position.is_some());
}
