use dbc_core::{Connection, QueryError};
use dbc_driver_postgres::PostgresConnection;
use dbc_driver_sqlite::SqliteConnection;

/// Dispatch a connection string to the right driver.
///
/// `postgres://` / `postgresql://` URLs go to the Postgres driver (connected
/// synchronously via `block_on` on the given runtime handle); anything else
/// is treated as a SQLite file path.
///
/// `block_on` here runs synchronously on the UI thread, once per `RunQuery`
/// dispatch (`AppView::on_run_query` calls this fresh for every query — v1
/// has no persistent connection cache). That's acceptable for phase 2; an
/// async connect UI with a connection manager is phase 4.
pub fn open(
    url: &str,
    runtime: &tokio::runtime::Handle,
) -> Result<Box<dyn Connection>, QueryError> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let url = url.to_owned();
        let conn = runtime.block_on(async move { PostgresConnection::connect(&url).await })?;
        Ok(Box::new(conn))
    } else {
        Ok(Box::new(SqliteConnection::new(url)))
    }
}
