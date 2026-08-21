use std::time::{Duration, Instant};

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{CancelToken, Connection, QueryError};

pub enum QueryEvent {
    Started { columns: SchemaRef },
    Batch(RecordBatch),
    Finished { elapsed: Duration },
    Failed(QueryError),
}

/// Owns the tokio runtime. All DB I/O lives here; the UI thread only ever
/// awaits the event channel from inside `cx.spawn`.
pub struct QueryRunner {
    runtime: tokio::runtime::Runtime,
}

impl QueryRunner {
    pub fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime"),
        }
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    pub fn run(
        &self,
        mut conn: Box<dyn Connection>,
        sql: String,
        cancel: CancelToken,
    ) -> tokio::sync::mpsc::Receiver<QueryEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        self.runtime.spawn(async move {
            let started = Instant::now();
            match conn.query(&sql, cancel).await {
                Err(e) => { let _ = tx.send(QueryEvent::Failed(e)).await; }
                Ok(mut stream) => {
                    let _ = tx.send(QueryEvent::Started { columns: stream.columns.clone() }).await;
                    let mut failed = false;
                    while let Some(item) = stream.batches.recv().await {
                        match item {
                            Ok(b) => { let _ = tx.send(QueryEvent::Batch(b)).await; }
                            Err(e) => { let _ = tx.send(QueryEvent::Failed(e)).await; failed = true; }
                        }
                    }
                    if !failed {
                        let _ = tx.send(QueryEvent::Finished { elapsed: started.elapsed() }).await;
                    }
                }
            }
        });
        rx
    }
}
