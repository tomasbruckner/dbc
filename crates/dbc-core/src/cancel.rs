/// Cooperative cancellation. Drivers watch this token and issue a
/// protocol-level cancel (pg CancelRequest / sqlite interrupt) when fired.
pub use tokio_util::sync::CancellationToken as CancelToken;
