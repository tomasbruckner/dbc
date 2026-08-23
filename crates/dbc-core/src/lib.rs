mod cancel;
mod connection;
mod ddl;
mod error;
mod guards;
mod params;
mod schema;
mod split;
mod stream;

pub use cancel::CancelToken;
pub use connection::Connection;
pub use ddl::{quote_ident, quote_qualified, synthesize_create_table};
pub use error::QueryError;
pub use guards::{apply_auto_limit, is_read_statement};
pub use params::{find_params, substitute_params};
pub use schema::{
    ColumnInfo, ConstraintInfo, FkRef, IndexInfo, RoutineInfo, RoutineKind, SchemaSnapshot,
    SequenceInfo, TableInfo, TableKind, TriggerInfo,
};
pub use split::{split_sql, Dialect, SplitError, StatementSplitter, UnterminatedKind};
pub use stream::{QueryStream, BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY};

// Re-export so drivers/UI use one arrow version.
pub use arrow;
