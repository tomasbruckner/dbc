//! Docker required. Run with: cargo test -p dbc-driver-postgres -- --ignored
use dbc_core::{CancelToken, Connection, RoutineKind, TableKind};
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

/// Full catalog snapshot: table+FK+index+view+matview+function+procedure+
/// trigger+sequence, mirroring T2's sqlite fixture (same shapes, exercised
/// against real Postgres catalogs this time).
#[tokio::test]
#[ignore]
async fn schema_returns_full_catalog_snapshot() {
    let node = Postgres::default().start().await.unwrap();
    let url = pg_url(&node).await;

    // Extended query protocol (what `PostgresConnection::query` uses via
    // `prepare()`) can't Parse a string containing multiple statements, so
    // the fixture DDL is loaded through a plain `tokio_postgres` connection
    // via `batch_execute` (simple query protocol) instead — mirrors how the
    // sqlite integration test builds its fixture directly through rusqlite
    // rather than through the driver under test.
    let (setup_client, setup_conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = setup_conn.await;
    });
    setup_client
        .batch_execute(
            "CREATE SEQUENCE cust_seq;
             CREATE TABLE customers (
                 id integer PRIMARY KEY DEFAULT nextval('cust_seq'),
                 name text NOT NULL
             );
             CREATE TABLE orders (
                 id integer PRIMARY KEY,
                 customer_id integer NOT NULL REFERENCES customers(id),
                 amount numeric DEFAULT 0,
                 note text
             );
             CREATE INDEX orders_customer_idx ON orders(customer_id);
             CREATE VIEW order_view AS
                 SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id;
             CREATE MATERIALIZED VIEW order_mv AS
                 SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id;
             CREATE FUNCTION add_nums(a integer, b integer) RETURNS integer AS
                 $$ SELECT a + b $$ LANGUAGE sql;
             CREATE PROCEDURE do_nothing() LANGUAGE sql AS $$ SELECT 1; $$;
             CREATE FUNCTION orders_touch() RETURNS trigger AS
                 $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
             CREATE TRIGGER orders_touch_trigger BEFORE INSERT ON orders
                 FOR EACH ROW EXECUTE FUNCTION orders_touch();
             CREATE TABLE posts (
                 id integer PRIMARY KEY,
                 title text NOT NULL,
                 body text
             );
             CREATE INDEX posts_lower_title_idx ON posts (lower(title));
             CREATE INDEX posts_mixed_idx ON posts (id, lower(body));",
        )
        .await
        .unwrap();

    let mut c = PostgresConnection::connect(&url).await.unwrap();
    let snap = c.schema().await.unwrap();

    let customers = snap.tables.iter().find(|t| t.name == "customers").unwrap();
    assert_eq!(customers.kind, TableKind::Table);
    assert_eq!(customers.ddl, None, "tables have no server-side ddl; UI synthesizes it");
    let cust_id = customers.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(cust_id.is_pk, "customers.id must be marked as the primary key");
    assert!(
        cust_id.default.as_deref().unwrap_or("").contains("nextval"),
        "customers.id default should reference the sequence, got: {:?}",
        cust_id.default
    );

    let orders = snap.tables.iter().find(|t| t.name == "orders").unwrap();
    assert_eq!(orders.kind, TableKind::Table);
    assert_eq!(orders.ddl, None);
    let order_id = orders.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(order_id.is_pk, "orders.id must be marked as the primary key");
    let cust_fk_col = orders.columns.iter().find(|c| c.name == "customer_id").unwrap();
    assert!(!cust_fk_col.nullable, "customer_id is NOT NULL");
    let fk = cust_fk_col.fk.as_ref().expect("customer_id should carry an FkRef");
    assert_eq!(fk.table, "customers");
    assert_eq!(fk.column, "id");
    assert_eq!(fk.schema.as_deref(), Some("public"));

    let idx = orders.indexes.iter().find(|i| i.name == "orders_customer_idx").unwrap();
    assert_eq!(idx.columns, vec!["customer_id".to_string()]);
    assert!(!idx.unique);

    let fk_constraint = orders.constraints.iter().find(|c| c.kind == "FOREIGN KEY").unwrap();
    assert!(fk_constraint.definition.contains("REFERENCES"));

    let view = snap.tables.iter().find(|t| t.name == "order_view").unwrap();
    assert_eq!(view.kind, TableKind::View);
    assert!(
        view.ddl.as_deref().unwrap_or("").contains("CREATE VIEW"),
        "view ddl: {:?}",
        view.ddl
    );

    let matview = snap.tables.iter().find(|t| t.name == "order_mv").unwrap();
    assert_eq!(matview.kind, TableKind::MaterializedView);
    assert!(
        matview.ddl.as_deref().unwrap_or("").contains("CREATE MATERIALIZED VIEW"),
        "matview ddl: {:?}",
        matview.ddl
    );

    let func = snap.routines.iter().find(|r| r.name == "add_nums").unwrap();
    assert_eq!(func.kind, RoutineKind::Function);
    assert!(
        func.ddl.as_deref().unwrap_or("").contains("CREATE OR REPLACE FUNCTION"),
        "function ddl: {:?}",
        func.ddl
    );
    assert!(func.signature.contains("integer"));

    let proc = snap.routines.iter().find(|r| r.name == "do_nothing").unwrap();
    assert_eq!(proc.kind, RoutineKind::Procedure);

    let trigger = snap.triggers.iter().find(|t| t.name == "orders_touch_trigger").unwrap();
    assert_eq!(trigger.table, "orders");
    assert!(
        trigger.ddl.as_deref().unwrap_or("").contains("CREATE TRIGGER"),
        "trigger ddl: {:?}",
        trigger.ddl
    );

    assert!(
        snap.sequences.iter().any(|s| s.name == "cust_seq"),
        "cust_seq should be listed among sequences"
    );

    // Expression/functional indexes: pure-expression and mixed plain+expr
    // must both be represented in full, in column order, rather than
    // dropped or truncated (indkey entries for expression columns are 0,
    // which has no backing pg_attribute row).
    let posts = snap.tables.iter().find(|t| t.name == "posts").unwrap();
    let expr_idx = posts.indexes.iter().find(|i| i.name == "posts_lower_title_idx").unwrap();
    assert_eq!(
        expr_idx.columns,
        vec!["lower(title)".to_string()],
        "pure expression index must render the expression text, not be dropped"
    );
    let mixed_idx = posts.indexes.iter().find(|i| i.name == "posts_mixed_idx").unwrap();
    assert_eq!(
        mixed_idx.columns,
        vec!["id".to_string(), "lower(body)".to_string()],
        "mixed plain+expression index must keep both columns, in order"
    );
}

/// A `CREATE TEMP TABLE` issued by a *different* concurrently-open session
/// creates a real `pg_class` row in a `pg_temp_N` namespace, visible to
/// every other backend's catalog queries for as long as that session's temp
/// schema exists. `schema()` must not surface it as a user table.
#[tokio::test]
#[ignore]
async fn temp_table_from_other_session_does_not_leak_into_schema() {
    let node = Postgres::default().start().await.unwrap();
    let url = pg_url(&node).await;

    // Second, independent session — kept open across the schema() call so
    // its pg_temp_N namespace still exists when the catalog is queried.
    let (temp_client, temp_conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = temp_conn.await;
    });
    temp_client
        .batch_execute("CREATE TEMP TABLE session_scratch (id integer, note text)")
        .await
        .unwrap();

    let mut c = PostgresConnection::connect(&url).await.unwrap();
    let snap = c.schema().await.unwrap();

    assert!(
        snap.tables.iter().all(|t| t.name != "session_scratch"),
        "a temp table from another session must not leak into the snapshot: {:?}",
        snap.tables.iter().map(|t| (t.schema.clone(), t.name.clone())).collect::<Vec<_>>()
    );

    // Keep the temp session alive until here so its temp schema existed for
    // the whole schema() call above.
    drop(temp_client);
}

/// `Connection::execute` affected-rows reporting, mirroring the sqlite
/// driver's `execute_reports_affected_rows` unit test.
#[tokio::test]
#[ignore]
async fn execute_reports_affected_rows() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();

    let n = c.execute("CREATE TABLE t(id integer, name text)", CancelToken::new()).await.unwrap();
    assert_eq!(n, 0);

    let n = c.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();
    assert_eq!(n, 1);
    let n = c.execute("INSERT INTO t(id, name) VALUES (2, 'b')", CancelToken::new()).await.unwrap();
    assert_eq!(n, 1);

    let n = c.execute("UPDATE t SET name = 'z'", CancelToken::new()).await.unwrap();
    assert_eq!(n, 2);

    let n = c.execute("DELETE FROM t WHERE id = 9999", CancelToken::new()).await.unwrap();
    assert_eq!(n, 0);
}

/// `BEGIN … ROLLBACK` driven through successive `execute` calls over the
/// SAME `PostgresConnection` must roll back — this is the write-path
/// contract Task 4's Apply runner depends on.
#[tokio::test]
#[ignore]
async fn execute_in_transaction_rolls_back() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    c.execute("CREATE TABLE t(id integer, name text)", CancelToken::new()).await.unwrap();

    c.execute("BEGIN", CancelToken::new()).await.unwrap();
    c.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();
    c.execute("ROLLBACK", CancelToken::new()).await.unwrap();

    let mut s = c.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
    let mut rows = 0usize;
    while let Some(b) = s.batches.recv().await {
        rows += b.unwrap().num_rows();
    }
    assert_eq!(rows, 0, "row inserted inside the rolled-back transaction must be absent");
}
