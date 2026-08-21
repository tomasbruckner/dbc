mod types;

use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::{Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, QueryError, QueryStream, SchemaSnapshot, TableInfo,
    BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY,
};
use futures_util::StreamExt;
use tokio_postgres::NoTls;
use types::{arrow_type, ColBuilder};

pub struct PostgresConnection {
    client: Arc<tokio_postgres::Client>,
}

fn pg_err(e: tokio_postgres::Error) -> QueryError {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code().to_string();
        QueryError {
            message: db.message().to_string(),
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            },
            code: Some(if code == "57014" { "cancelled".into() } else { code }),
        }
    } else {
        QueryError::msg(e.to_string())
    }
}

impl PostgresConnection {
    pub async fn connect(url: &str) -> Result<Self, QueryError> {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await.map_err(pg_err)?;
        // The connection object drives the socket; it must be polled.
        tokio::spawn(async move {
            let _ = connection.await; // errors surface on the client side
        });
        Ok(Self { client: Arc::new(client) })
    }
}

#[async_trait]
impl Connection for PostgresConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        // prepare() gives us column names AND types before the first row,
        // and resolves as soon as Parse/Describe come back — independent of
        // how long the query itself takes to run.
        let stmt = self.client.prepare(sql).await.map_err(pg_err)?;
        let fields: Vec<Field> = stmt
            .columns()
            .iter()
            .map(|c| Field::new(c.name(), arrow_type(c.type_()), true))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let col_types: Vec<tokio_postgres::types::Type> =
            stmt.columns().iter().map(|c| c.type_().clone()).collect();

        // Protocol-level cancel goes over a separate connection. The watcher
        // must not outlive the query: once it's done (normally, on error, or
        // because the consumer dropped the stream), a `done_tx` drop races
        // against `cancelled()` so the watcher task exits either way. Without
        // this, the watcher lives until the CancelToken is cancelled (often
        // never), and a *late* cancel() — fired well after this query
        // finished — would still send a CancelRequest carrying this
        // connection's backend process id, potentially killing an unrelated
        // query that's since started using the same connection.
        let cancel_handle = self.client.cancel_token();
        let watcher_cancel = cancel.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = watcher_cancel.cancelled() => {
                    let _ = cancel_handle.cancel_query(NoTls).await;
                }
                _ = done_rx => {
                    // Query already finished; nothing to cancel.
                }
            }
        });

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let batch_schema = schema.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            // Keep `done_tx` alive for exactly this task's lifetime; its
            // drop (on any exit path below) wakes the watcher above.
            let _done_tx = done_tx;
            // IMPORTANT: query_raw() itself is awaited *inside* this task,
            // not in the `query()` method above. Bind/Execute/Sync are sent
            // as a single batch, and Postgres only flushes its response
            // buffer (BindComplete/DataRow/.../ReadyForQuery) once it has
            // processed Sync — which happens only after Execute finishes
            // running the query server-side. So query_raw().await blocks
            // until the query has essentially completed, not just until
            // streaming starts. Awaiting it here (rather than in `query()`)
            // keeps `query()` non-blocking, so the caller gets the
            // QueryStream back immediately, can react to the header right
            // away, and can still cancel a long-running query — cancelling
            // before this await resolves makes it fail with a "cancelled"
            // QueryError instead of hanging for the query's full duration.
            let params: Vec<String> = Vec::new();
            let row_stream = match client.query_raw(&stmt, params).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(pg_err(e))).await;
                    return;
                }
            };
            // RowStream is !Unpin (contains PhantomPinned); pin it in this
            // task's stack frame so StreamExt::next() can be called on it.
            tokio::pin!(row_stream);
            let new_builders = |types: &[tokio_postgres::types::Type]| -> Vec<ColBuilder> {
                types.iter().map(ColBuilder::for_type).collect()
            };
            let mut builders = new_builders(&col_types);
            let mut in_batch = 0usize;
            let mut deadline: Option<tokio::time::Instant> = None;

            loop {
                let next = if let Some(d) = deadline {
                    tokio::select! {
                        r = row_stream.next() => Some(r),
                        _ = tokio::time::sleep_until(d) => None, // latency flush
                    }
                } else {
                    Some(row_stream.next().await)
                };

                let flush_now = match next {
                    None => true, // 16ms deadline hit
                    Some(None) => { // stream done
                        if in_batch > 0 {
                            let arrays = builders.iter_mut().map(|b| b.finish()).collect();
                            if let Ok(b) = RecordBatch::try_new(batch_schema.clone(), arrays) {
                                let _ = tx.send(Ok(b)).await;
                            }
                        }
                        break;
                    }
                    Some(Some(Err(e))) => {
                        let _ = tx.send(Err(pg_err(e))).await;
                        break;
                    }
                    Some(Some(Ok(row))) => {
                        for (i, b) in builders.iter_mut().enumerate() {
                            b.append(&row, i);
                        }
                        in_batch += 1;
                        if in_batch == 1 {
                            deadline = Some(tokio::time::Instant::now() + BATCH_LATENCY);
                        }
                        in_batch >= BATCH_ROWS
                    }
                };

                if flush_now && in_batch > 0 {
                    let arrays = builders.iter_mut().map(|b| b.finish()).collect();
                    match RecordBatch::try_new(batch_schema.clone(), arrays) {
                        Ok(b) => {
                            if tx.send(Ok(b)).await.is_err() { break; } // consumer gone
                        }
                        Err(e) => {
                            let _ = tx.send(Err(QueryError::msg(e.to_string()))).await;
                            break;
                        }
                    }
                    builders = new_builders(&col_types);
                    in_batch = 0;
                    deadline = None;
                }
            }
        });

        Ok(QueryStream { columns: schema, batches: rx })
    }

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let rows = self
            .client
            .query(
                "SELECT table_schema, table_name, column_name, data_type
                 FROM information_schema.columns
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
                 ORDER BY table_schema, table_name, ordinal_position",
                &[],
            )
            .await
            .map_err(pg_err)?;
        let mut tables: Vec<TableInfo> = Vec::new();
        for row in rows {
            let (ts, tn): (String, String) = (row.get(0), row.get(1));
            let col = ColumnInfo { name: row.get(2), data_type: row.get(3) };
            match tables.last_mut() {
                Some(t) if t.schema.as_deref() == Some(&ts) && t.name == tn => t.columns.push(col),
                _ => tables.push(TableInfo { schema: Some(ts), name: tn, columns: vec![col] }),
            }
        }
        Ok(SchemaSnapshot { tables })
    }
}
