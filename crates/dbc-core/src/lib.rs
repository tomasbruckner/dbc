mod cancel;
mod connection;
mod ddl;
pub mod erd;
mod error;
mod guards;
mod params;
mod schema;
mod split;
mod stream;
mod tx;

pub use cancel::CancelToken;
pub use connection::Connection;
pub use ddl::{
    quote_ident, quote_ident_d, quote_qualified, quote_qualified_d, synthesize_create_table,
    synthesize_create_table_d,
};
pub use error::QueryError;
pub use guards::{apply_auto_limit, apply_auto_limit_d, is_read_statement};
pub use params::{find_params, substitute_params};
pub use schema::{
    ColumnInfo, ConstraintInfo, FkRef, IndexInfo, RoutineInfo, RoutineKind, SchemaSnapshot,
    SequenceInfo, TableInfo, TableKind, TriggerInfo,
};
pub use split::{split_sql, Dialect, SplitError, StatementSplitter, UnterminatedKind};
pub use stream::{QueryStream, BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY};
pub use tx::{tx_begin_sql, tx_commit_sql, tx_rollback_sql};

// Re-export so drivers/UI use one arrow version.
pub use arrow;
