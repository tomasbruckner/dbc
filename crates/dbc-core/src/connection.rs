use async_trait::async_trait;
use crate::{cancel::CancelToken, error::QueryError, schema::SchemaSnapshot, stream::QueryStream};

#[async_trait]
pub trait Connection: Send {
    /// Contract on the returned `QueryStream::batches` channel: once the
    /// driver sends an `Err(QueryError)` batch, it MUST stop sending any
    /// further items and drop its `Sender` so the channel closes. Consumers
    /// (e.g. the UI's query runner) rely on this to treat "stream ended
    /// after an Err" as the terminal state for that error — they do not
    /// keep draining past it. A driver that sends more `Ok` batches after
    /// an `Err`, or that fails to drop the sender, can cause a consumer
    /// that stops on first Err to leave the sender's task blocked forever
    /// on a full channel, or to interleave post-error data with an
    /// already-reported failure.
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError>;
    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError>;
}
