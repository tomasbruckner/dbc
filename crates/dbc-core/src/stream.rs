use std::time::Duration;
use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use crate::error::QueryError;

pub const BATCH_ROWS: usize = 1024;
pub const BATCH_LATENCY: Duration = Duration::from_millis(16);
pub const CHANNEL_CAPACITY: usize = 8;

/// Columns are known before the first row so the UI can draw its header
/// immediately. Batches arrive columnar; the bounded channel provides
/// backpressure against a slow consumer.
#[derive(Debug)]
pub struct QueryStream {
    pub columns: SchemaRef,
    pub batches: tokio::sync::mpsc::Receiver<Result<RecordBatch, QueryError>>,
}
