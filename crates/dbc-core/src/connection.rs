use async_trait::async_trait;
use crate::{cancel::CancelToken, error::QueryError, schema::SchemaSnapshot, stream::QueryStream};

#[async_trait]
pub trait Connection: Send {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError>;
    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError>;
}
