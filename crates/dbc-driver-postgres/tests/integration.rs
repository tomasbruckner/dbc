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
    let err = c.query("SELEC 1", CancelToken::new()).await.err().unwrap();
    assert_eq!(err.code.as_deref(), Some("42601"));
    assert!(err.position.is_some());
}

#[tokio::test]
#[ignore]
async fn null_in_fallback_type_renders_as_null_not_placeholder() {
    use dbc_core::arrow::array::{Array, StringArray};
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let mut s = c
        .query("SELECT NULL::interval AS a, '1 day'::interval AS b", CancelToken::new())
        .await
        .unwrap();
    let batch = s.batches.recv().await.unwrap().unwrap();
    let a = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    let b = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    assert!(a.is_null(0), "NULL interval must render as null, not the oid placeholder");
    assert_eq!(b.value(0), "<oid 1186>");
}

#[tokio::test]
#[ignore]
async fn stale_cancel_after_completion_does_not_kill_later_query() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();

    // Run a query to completion and drain it fully.
    let cancel1 = CancelToken::new();
    let mut s1 = c.query("SELECT 1", cancel1.clone()).await.unwrap();
    while let Some(r) = s1.batches.recv().await {
        r.unwrap();
    }
    // Give the watcher task a moment to observe completion and exit.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // A cancel fired well after the query finished must be a no-op: the
    // watcher for that query should already be gone, so this must not send
    // a CancelRequest that could hit a later query on the same connection.
    cancel1.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // A brand new query on the same connection must succeed normally.
    let mut s2 = c.query("SELECT 2", CancelToken::new()).await.unwrap();
    match s2.batches.recv().await.unwrap() {
        Ok(_) => {}
        Err(e) => panic!("unrelated query was killed by a stale cancel: {e:?}"),
    }
}

/// NUMERIC 'NaN' has no `rust_decimal` representation, and 'infinity' /
/// '-infinity' have no `chrono` representation — both are legal Postgres
/// values that used to panic inside the streaming task via `row.get`
/// (a panic there silently ends the stream: the sender task dies, the
/// channel closes cleanly, and the UI reports SUCCESS with truncated rows).
/// After the fix these decode as a placeholder string instead of panicking,
/// and the row is not dropped.
#[tokio::test]
#[ignore]
async fn decode_hazards_render_as_placeholders() {
    use dbc_core::arrow::array::{Array, Int32Array, StringArray};
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let mut s = c
        .query(
            "SELECT 'NaN'::numeric AS a, 'infinity'::timestamp AS b, '-infinity'::date AS c, 1 AS ok",
            CancelToken::new(),
        )
        .await
        .unwrap();

    let mut rows = 0usize;
    let mut last_batch = None;
    while let Some(item) = s.batches.recv().await {
        let b = item.unwrap_or_else(|e| panic!("stream failed instead of rendering a placeholder: {e:?}"));
        rows += b.num_rows();
        last_batch = Some(b);
    }
    assert_eq!(rows, 1, "row must not be dropped/truncated by a decode panic");

    let batch = last_batch.expect("expected one batch");
    let a = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    let b = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    let c_col = batch.column(2).as_any().downcast_ref::<StringArray>().unwrap();
    let ok = batch.column(3).as_any().downcast_ref::<Int32Array>().unwrap();

    assert_eq!(ok.value(0), 1, "unrelated column must read correctly");
    assert!(
        !a.is_null(0) && !a.value(0).is_empty(),
        "numeric NaN must render as a non-null placeholder, got: {:?}",
        a.is_null(0).then_some("<null>").unwrap_or(a.value(0))
    );
    assert!(
        !b.is_null(0) && !b.value(0).is_empty(),
        "infinity timestamp must render as a non-null placeholder, got: {:?}",
        b.is_null(0).then_some("<null>").unwrap_or(b.value(0))
    );
    assert!(
        !c_col.is_null(0) && !c_col.value(0).is_empty(),
        "-infinity date must render as a non-null placeholder, got: {:?}",
        c_col.is_null(0).then_some("<null>").unwrap_or(c_col.value(0))
    );
    eprintln!(
        "decode_hazards_render_as_placeholders actual values: a={:?} b={:?} c={:?} ok={}",
        a.value(0),
        b.value(0),
        c_col.value(0),
        ok.value(0)
    );
}
