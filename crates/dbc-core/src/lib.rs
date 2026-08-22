mod cancel;
mod connection;
mod error;
mod guards;
mod schema;
mod stream;

pub use cancel::CancelToken;
pub use connection::Connection;
pub use error::QueryError;
pub use guards::{apply_auto_limit, is_read_statement};
pub use schema::{ColumnInfo, SchemaSnapshot, TableInfo};
pub use stream::{QueryStream, BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY};

// Re-export so drivers/UI use one arrow version.
pub use arrow;
