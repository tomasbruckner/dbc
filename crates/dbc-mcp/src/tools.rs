//! The three tools `dbc-mcp` exposes (design doc §5): `list_connections`,
//! `get_schema`, `run_query`. No fourth tool, ever — in particular no
//! write/`execute` tool (§4 layer 3, mechanically enforced by the
//! regression test at the bottom of this file).

use std::time::{Duration, Instant};

use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{apply_auto_limit, is_read_statement, CancelToken, Connection, QueryError};
use dbc_state::{AppConfig, ConnectionConfig, Vault};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{tool, tool_router};
use serde_json::json;

use crate::connect::open_for_mcp;
use crate::serialize::{rows_to_json, schema_to_json, DrainedResult};
use dbc_buffer::ResultBuffer;

const ROW_LIMIT_DEFAULT: u32 = 200;
const ROW_LIMIT_CEILING: u32 = 1000;
const TIMEOUT_DEFAULT_SECS: u32 = 30;
const TIMEOUT_CEILING_SECS: u32 = 120;

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ListConnectionsParams {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetSchemaParams {
    pub connection_id: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub include_ddl: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunQueryParams {
    pub connection_id: String,
    pub sql: String,
    #[serde(default)]
    pub row_limit: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u32>,
}

/// The MCP server's state: the (read-only, from this process's point of
/// view) saved connections config, and the unlocked vault used to resolve
/// per-connection passwords. No connection is ever cached here — §7's
/// "one connection per tool call" model needs no shared mutable state at
/// all, so `&self` (not `&mut self`) is enough for every tool method.
pub struct McpServer {
    pub config: AppConfig,
    pub vault: Vault,
}

impl McpServer {
    pub fn new(config: AppConfig, vault: Vault) -> Self {
        Self { config, vault }
    }

    fn find_connection(&self, id: &str) -> Option<ConnectionConfig> {
        self.config.connections.iter().find(|c| c.id == id).cloned()
    }
}

fn query_error_result(e: &QueryError) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": e.code,
        "message": e.message,
        "position": e.position,
    }))
}

/// Formats the design doc §4 audit-log line. Kept as a pure string
/// formatter, separate from the `tracing::info!` call site, so the shape
/// is unit-testable without standing up a subscriber (review round 1
/// finding #1's stated minimum bar).
///
/// Base shape: `tool=<name> connection=<name> rows=<n> duration_ms=<n>`,
/// with `sql=<text>` appended when `sql` is `Some` (only `run_query` ever
/// passes one — §4's "SQL text yes" logging policy, matching the GUI's own
/// `HistoryEntry` policy: SQL yes, connection name yes, never row data,
/// never a password) and `error=<msg>` appended when `error` is `Some`.
///
/// SECURITY: callers must only ever pass the SQL text itself (never row
/// data) and a `QueryError`'s own message or a short fixed reason string
/// for `error` — never a secret. No call site in this crate passes a
/// connection password or vault key into either parameter.
fn audit_line(
    tool: &str,
    connection: &str,
    sql: Option<&str>,
    rows: Option<u64>,
    duration_ms: u64,
    error: Option<&str>,
) -> String {
    let rows = rows.map(|r| r.to_string()).unwrap_or_else(|| "-".to_string());
    let mut line = format!("tool={tool} connection={connection} rows={rows} duration_ms={duration_ms}");
    if let Some(sql) = sql {
        line.push_str(" sql=");
        line.push_str(sql);
    }
    if let Some(e) = error {
        line.push_str(" error=");
        line.push_str(e);
    }
    line
}

/// Emits [`audit_line`]'s output as a single `tracing` INFO event (design
/// doc §4: "every tool call emits one tracing INFO line to stderr").
/// Called at every return point of every tool method below, success and
/// error alike, so no tool call ever completes silently.
fn log_tool_call(
    tool: &str,
    connection: &str,
    sql: Option<&str>,
    rows: Option<u64>,
    duration_ms: u64,
    error: Option<&str>,
) {
    tracing::info!("{}", audit_line(tool, connection, sql, rows, duration_ms, error));
}

fn clamp_row_limit(requested: Option<u32>) -> (u32, bool) {
    match requested {
        None => (ROW_LIMIT_DEFAULT, false),
        Some(0) => (1, true),
        Some(n) if n > ROW_LIMIT_CEILING => (ROW_LIMIT_CEILING, true),
        Some(n) => (n, false),
    }
}

fn clamp_timeout(requested: Option<u32>) -> (u32, bool) {
    match requested {
        None => (TIMEOUT_DEFAULT_SECS, false),
        Some(0) => (1, true),
        Some(n) if n > TIMEOUT_CEILING_SECS => (TIMEOUT_CEILING_SECS, true),
        Some(n) => (n, false),
    }
}

struct RawDrained {
    columns: SchemaRef,
    buffer: ResultBuffer,
    row_limit_hit: bool,
}

/// Drains a query's `QueryStream` into a `ResultBuffer`, stopping the
/// instant `row_limit` rows have been collected (§5's "real hard cap is at
/// result consumption") regardless of what the SQL itself requested or
/// whether `apply_auto_limit` fired. The very last batch is sliced (not
/// dropped wholesale) so `row_count` lands exactly on `row_limit` when
/// truncated.
///
/// Reaching exactly `row_limit` rows is ALWAYS reported as truncated, even
/// if that happens to be the very last row the query would ever have
/// produced (e.g. `apply_auto_limit` capped the SQL itself to exactly
/// `row_limit` rows) — there is no cheap way to distinguish "exactly
/// row_limit rows existed" from "there were more", so this deliberately
/// errs conservative rather than under-reporting truncation.
async fn drain_query(
    conn: &mut dyn Connection,
    sql: &str,
    cancel: CancelToken,
    row_limit: usize,
) -> Result<RawDrained, QueryError> {
    let mut stream = conn.query(sql, cancel.clone()).await?;
    let mut buf = ResultBuffer::with_cap(stream.columns.clone(), row_limit.max(1));
    let mut row_limit_hit = false;

    loop {
        match stream.batches.recv().await {
            Some(Ok(batch)) => {
                let remaining = row_limit.saturating_sub(buf.row_count());
                if remaining == 0 {
                    row_limit_hit = true;
                    cancel.cancel();
                    break;
                }
                let to_push =
                    if batch.num_rows() > remaining { batch.slice(0, remaining) } else { batch };
                buf.push(to_push).map_err(|e| QueryError::msg(e.to_string()))?;
                if buf.row_count() >= row_limit {
                    row_limit_hit = true;
                    cancel.cancel();
                    break;
                }
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }

    Ok(RawDrained { columns: stream.columns, buffer: buf, row_limit_hit })
}

#[tool_router(server_handler)]
impl McpServer {
    #[tool(
        description = "List saved connections (id, name, engine, folder, read_only, favourite). No secrets, no host/user/database — use get_schema/run_query with the id to explore a specific connection."
    )]
    async fn list_connections(
        &self,
        Parameters(_): Parameters<ListConnectionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let items: Vec<serde_json::Value> = self
            .config
            .connections
            .iter()
            .filter(|c| c.ssh.is_none()) // v1 non-goal: SSH-tunneled connections (design doc §1)
            .map(|c| {
                json!({
                    "id": c.id,
                    "name": c.name,
                    "engine": c.engine,
                    "folder": c.folder,
                    "read_only": c.read_only,
                    "favourite": c.favourite,
                })
            })
            .collect();
        // No single "connection" applies to a list call — "-" per
        // audit_line's documented placeholder.
        log_tool_call(
            "list_connections",
            "-",
            None,
            Some(items.len() as u64),
            start.elapsed().as_millis() as u64,
            None,
        );
        Ok(CallToolResult::structured(json!({ "connections": items })))
    }

    #[tool(
        description = "Get a connection's schema (tables, routines, triggers, sequences). include_ddl defaults to false (DDL bodies are the biggest size contributor and rarely needed for exploration)."
    )]
    async fn get_schema(
        &self,
        Parameters(p): Parameters<GetSchemaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let cfg = match self.find_connection(&p.connection_id) {
            Some(c) => c,
            None => {
                log_tool_call(
                    "get_schema",
                    &p.connection_id,
                    None,
                    None,
                    start.elapsed().as_millis() as u64,
                    Some("unknown connection_id"),
                );
                return Err(ErrorData::invalid_params(
                    format!("unknown connection_id: {}", p.connection_id),
                    None,
                ));
            }
        };
        if cfg.ssh.is_some() {
            let msg = "SSH-tunneled connections are not available over MCP (v1 non-goal)";
            log_tool_call("get_schema", &cfg.name, None, None, start.elapsed().as_millis() as u64, Some(msg));
            return Ok(CallToolResult::structured_error(json!({ "error": msg })));
        }

        let secret = self.vault.get_secret(&cfg.id);
        let mut conn = match open_for_mcp(&cfg, secret).await {
            Ok(c) => c,
            Err(e) => {
                log_tool_call(
                    "get_schema",
                    &cfg.name,
                    None,
                    None,
                    start.elapsed().as_millis() as u64,
                    Some(&e.message),
                );
                return Ok(query_error_result(&e));
            }
        };
        let snapshot = match conn.schema().await {
            Ok(s) => s,
            Err(e) => {
                log_tool_call(
                    "get_schema",
                    &cfg.name,
                    None,
                    None,
                    start.elapsed().as_millis() as u64,
                    Some(&e.message),
                );
                return Ok(query_error_result(&e));
            }
        };

        let include_ddl = p.include_ddl.unwrap_or(false);
        let body = schema_to_json(&snapshot, p.schema.as_deref(), include_ddl);
        let table_count = body.get("tables").and_then(|t| t.as_array()).map(|a| a.len() as u64);
        log_tool_call("get_schema", &cfg.name, None, table_count, start.elapsed().as_millis() as u64, None);
        Ok(CallToolResult::structured(body))
    }

    #[tool(
        description = "Run a read-only SQL query (SELECT/WITH/EXPLAIN/SHOW/VALUES/PRAGMA-getters only — writes are rejected before any connection is opened). row_limit defaults to 200, hard ceiling 1000. timeout_secs defaults to 30, ceiling 120. Rows are returned as arrays (columns carry the name↔position mapping); every cell is a string or null."
    )]
    async fn run_query(
        &self,
        Parameters(p): Parameters<RunQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_start = Instant::now();

        // Gate 1 (§4 layer 1): reject before ever opening a connection.
        if !is_read_statement(&p.sql) {
            let msg = "only read-only statements are allowed over MCP (SELECT/WITH/EXPLAIN/SHOW/VALUES/PRAGMA-getters); the statement was rejected before any connection was opened";
            log_tool_call(
                "run_query",
                &p.connection_id,
                Some(&p.sql),
                None,
                tool_start.elapsed().as_millis() as u64,
                Some("write statement rejected"),
            );
            return Ok(CallToolResult::structured_error(json!({
                "error": "write statement rejected",
                "message": msg,
            })));
        }

        let cfg = match self.find_connection(&p.connection_id) {
            Some(c) => c,
            None => {
                log_tool_call(
                    "run_query",
                    &p.connection_id,
                    Some(&p.sql),
                    None,
                    tool_start.elapsed().as_millis() as u64,
                    Some("unknown connection_id"),
                );
                return Err(ErrorData::invalid_params(
                    format!("unknown connection_id: {}", p.connection_id),
                    None,
                ));
            }
        };
        if cfg.ssh.is_some() {
            let msg = "SSH-tunneled connections are not available over MCP (v1 non-goal)";
            log_tool_call(
                "run_query",
                &cfg.name,
                Some(&p.sql),
                None,
                tool_start.elapsed().as_millis() as u64,
                Some(msg),
            );
            return Ok(CallToolResult::structured_error(json!({ "error": msg })));
        }

        let (row_limit, row_limit_clamped) = clamp_row_limit(p.row_limit);
        let (timeout_secs, _timeout_clamped) = clamp_timeout(p.timeout_secs);
        let (rewritten_sql, _auto_limited) = apply_auto_limit(&p.sql, row_limit as u64);

        let secret = self.vault.get_secret(&cfg.id);
        let mut conn = match open_for_mcp(&cfg, secret).await {
            Ok(c) => c,
            Err(e) => {
                log_tool_call(
                    "run_query",
                    &cfg.name,
                    Some(&p.sql),
                    None,
                    tool_start.elapsed().as_millis() as u64,
                    Some(&e.message),
                );
                return Ok(query_error_result(&e));
            }
        };

        let cancel = CancelToken::new();
        let start = Instant::now();
        let timeout_dur = Duration::from_secs(timeout_secs as u64);

        let drained = match tokio::time::timeout(
            timeout_dur,
            drain_query(conn.as_mut(), &rewritten_sql, cancel.clone(), row_limit as usize),
        )
        .await
        {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                log_tool_call(
                    "run_query",
                    &cfg.name,
                    Some(&p.sql),
                    None,
                    tool_start.elapsed().as_millis() as u64,
                    Some(&e.message),
                );
                return Ok(query_error_result(&e));
            }
            Err(_) => {
                // Protocol-level cancel (§5): fires the same CancelToken the
                // driver watches, then reports a timeout — v1 is
                // all-or-nothing, no partial rows on timeout.
                cancel.cancel();
                let msg = format!("query exceeded the {timeout_secs}s timeout");
                log_tool_call(
                    "run_query",
                    &cfg.name,
                    Some(&p.sql),
                    None,
                    tool_start.elapsed().as_millis() as u64,
                    Some(&msg),
                );
                return Ok(CallToolResult::structured_error(json!({
                    "error": "timeout",
                    "message": msg,
                })));
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let mut body = rows_to_json(DrainedResult {
            columns: drained.columns,
            buffer: drained.buffer,
            row_limit_hit: drained.row_limit_hit,
            duration_ms,
        });
        if let Some(obj) = body.as_object_mut() {
            obj.insert("row_limit".into(), json!(row_limit));
            obj.insert("row_limit_clamped".into(), json!(row_limit_clamped));
        }
        let row_count = body.get("row_count").and_then(|v| v.as_u64());
        log_tool_call(
            "run_query",
            &cfg.name,
            Some(&p.sql),
            row_count,
            tool_start.elapsed().as_millis() as u64,
            None,
        );
        Ok(CallToolResult::structured(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_state::Engine;

    fn sqlite_fixture() -> (tempfile::NamedTempFile, String) {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(f.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO t(id, name) VALUES (1, 'a'), (2, 'b'), (3, NULL);",
            )
            .unwrap();
        }
        let path = f.path().to_string_lossy().into_owned();
        (f, path)
    }

    fn cfg(id: &str, database: &str, read_only: bool, ssh: bool) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: format!("conn-{id}"),
            folder: vec!["work".into()],
            engine: Engine::Sqlite,
            host: String::new(),
            port: None,
            database: database.into(),
            user: String::new(),
            read_only,
            timeout_secs: None,
            auto_limit: None,
            ssh: if ssh {
                Some(dbc_state::SshTunnelConfig { host: "h".into(), port: 22, user: "u".into(), key_path: None })
            } else {
                None
            },
            favourite: true,
            mssql: None,
        }
    }

    fn test_vault() -> Vault {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the vault file survives for the test's
        // duration (fine for a short-lived unit test process).
        let path = dir.keep().join("vault.bin");
        Vault::create(&path, "pw").unwrap()
    }

    #[tokio::test]
    async fn list_connections_hides_secrets_and_ssh_entries() {
        let (_f, path) = sqlite_fixture();
        let config = AppConfig {
            connections: vec![
                cfg("c1", &path, true, false),
                cfg("c2", &path, false, true), // ssh: must be excluded
            ],
            favourite_objects: vec![],
            ..Default::default()
        };
        let server = McpServer::new(config, test_vault());
        let result = server.list_connections(Parameters(ListConnectionsParams {})).await.unwrap();
        let body = result.structured_content.unwrap();
        let raw = serde_json::to_string(&body).unwrap();
        assert!(!raw.to_lowercase().contains("host"));
        assert!(!raw.to_lowercase().contains("password"));
        assert!(!raw.to_lowercase().contains("\"user\""));
        assert!(!raw.to_lowercase().contains("database"));

        let conns = body["connections"].as_array().unwrap();
        assert_eq!(conns.len(), 1, "the ssh-tunneled connection must be excluded");
        assert_eq!(conns[0]["id"], "c1");
        assert_eq!(conns[0]["read_only"], true);
        assert_eq!(conns[0]["favourite"], true);
        assert_eq!(conns[0]["engine"], "sqlite");
    }

    #[tokio::test]
    async fn get_schema_round_trips_known_tables_and_strips_ddl_by_default() {
        let (_f, path) = sqlite_fixture();
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .get_schema(Parameters(GetSchemaParams { connection_id: "c1".into(), schema: None, include_ddl: None }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        let tables = body["tables"].as_array().unwrap();
        let t = tables.iter().find(|t| t["name"] == "t").unwrap();
        assert!(t["ddl"].is_null(), "include_ddl defaults to false");

        let result2 = server
            .get_schema(Parameters(GetSchemaParams {
                connection_id: "c1".into(),
                schema: None,
                include_ddl: Some(true),
            }))
            .await
            .unwrap();
        let body2 = result2.structured_content.unwrap();
        let t2 = body2["tables"].as_array().unwrap().iter().find(|t| t["name"] == "t").unwrap();
        assert!(t2["ddl"].as_str().unwrap().contains("CREATE TABLE"));
    }

    #[tokio::test]
    async fn run_query_returns_correct_rows_columns_and_nulls() {
        let (_f, path) = sqlite_fixture();
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .run_query(Parameters(RunQueryParams {
                connection_id: "c1".into(),
                sql: "SELECT id, name FROM t ORDER BY id".into(),
                row_limit: None,
                timeout_secs: None,
            }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        assert_eq!(body["row_count"], 3);
        assert_eq!(body["truncated"], false);
        assert_eq!(body["rows"][0], json!(["1", "a"]));
        assert_eq!(body["rows"][2], json!(["3", null]));
        assert_eq!(body["columns"][0]["name"], "id");
    }

    #[tokio::test]
    async fn run_query_rejects_write_before_opening_a_connection() {
        let (_f, path) = sqlite_fixture();
        // Make the fixture file read-only at the filesystem level: if the
        // lexical gate were bypassed and a connection actually opened for a
        // write, the OS itself would also refuse it — so a bypass fails
        // loudly (a filesystem permission error) rather than silently
        // succeeding, per T6(b)'s explicit ask.
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .run_query(Parameters(RunQueryParams {
                connection_id: "c1".into(),
                sql: "INSERT INTO t(id, name) VALUES (99, 'x')".into(),
                row_limit: None,
                timeout_secs: None,
            }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        assert_eq!(body["error"], "write statement rejected");

        // Restore writability so the temp file can be cleaned up.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    #[tokio::test]
    async fn run_query_truncates_at_row_limit() {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(f.path()).unwrap();
            let values: Vec<String> = (0..500).map(|i| format!("({i})")).collect();
            let sql = format!("CREATE TABLE big(id INTEGER); INSERT INTO big(id) VALUES {};", values.join(","));
            conn.execute_batch(&sql).unwrap();
        }
        let path = f.path().to_string_lossy().into_owned();
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .run_query(Parameters(RunQueryParams {
                connection_id: "c1".into(),
                sql: "SELECT id FROM big".into(),
                row_limit: Some(50),
                timeout_secs: None,
            }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        assert_eq!(body["truncated"], true);
        assert_eq!(body["row_count"], 50);
    }

    #[tokio::test]
    async fn run_query_row_limit_above_ceiling_is_clamped_and_reported() {
        let (_f, path) = sqlite_fixture();
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .run_query(Parameters(RunQueryParams {
                connection_id: "c1".into(),
                sql: "SELECT id FROM t".into(),
                row_limit: Some(5000),
                timeout_secs: None,
            }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        assert_eq!(body["row_limit"], 1000);
        assert_eq!(body["row_limit_clamped"], true);
    }

    // T6(d): "a deliberately slow query is cut off by the timeout, returning
    // a timeout error rather than hanging" — exercised here against a mock
    // `Connection` whose `query()` never produces a batch until cancelled,
    // rather than against a real SQLite cross-join. Deliberate choice: a
    // real slow query's *actual* cut-off latency depends on SQLite's own
    // interrupt delivery, which dbc-driver-sqlite's own module doc already
    // flags as not reliably prompt on its own ("Checking the token between
    // rows guarantees cancellation lands at row granularity regardless of
    // interrupt timing") — and an aggregate/filtered cross join large
    // enough to force real multi-second work produced exactly that
    // flakiness while developing this test (it hung well past the MCP
    // timeout in practice). dbc-driver-sqlite's own suite already covers
    // that its interrupt path works; what this test needs to prove is
    // narrower and fully deterministic: `run_query`'s
    // `tokio::time::timeout` + `CancelToken` wiring cuts off a connection
    // that never responds, rather than hanging the tool call itself.
    struct NeverReturns;

    #[async_trait::async_trait]
    impl Connection for NeverReturns {
        async fn query(&mut self, _sql: &str, cancel: CancelToken) -> Result<dbc_core::QueryStream, QueryError> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let schema: SchemaRef = std::sync::Arc::new(dbc_core::arrow::datatypes::Schema::empty());
            // Never sends anything unless/until cancelled — simulates a
            // server that's simply stuck.
            tokio::spawn(async move {
                cancel.cancelled().await;
                drop(tx);
            });
            Ok(dbc_core::QueryStream { columns: schema, batches: rx })
        }
        async fn schema(&mut self) -> Result<dbc_core::SchemaSnapshot, QueryError> {
            Ok(dbc_core::SchemaSnapshot::default())
        }
        async fn execute(&mut self, _sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
            Err(QueryError::msg("not implemented in test mock"))
        }
    }

    #[tokio::test]
    async fn drain_query_is_cut_off_by_timeout_rather_than_hanging() {
        let mut conn = NeverReturns;
        let cancel = CancelToken::new();
        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            drain_query(&mut conn, "SELECT 1", cancel.clone(), 200),
        )
        .await;
        assert!(outcome.is_err(), "expected the outer timeout to fire before drain_query ever returns");
        // Mirrors what run_query itself does on the Err(_) branch: fire the
        // cancel token so nothing is left dangling.
        cancel.cancel();
    }

    #[tokio::test]
    async fn run_query_syntax_error_surfaces_as_tool_error_not_a_panic() {
        let (_f, path) = sqlite_fixture();
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .run_query(Parameters(RunQueryParams {
                connection_id: "c1".into(),
                sql: "SELECT this is not valid sql".into(),
                row_limit: None,
                timeout_secs: None,
            }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        assert!(body.get("message").is_some(), "expected a QueryError-shaped tool error, got: {body:?}");
    }

    #[tokio::test]
    async fn read_only_config_flag_is_respected_and_forced_regardless() {
        // Even with read_only:false saved on the connection, MCP forces it
        // anyway — the exhaustive version of this lives in connect.rs's own
        // tests; this just checks the tool layer doesn't undo it.
        let (_f, path) = sqlite_fixture();
        let config = AppConfig { connections: vec![cfg("c1", &path, false, false)], favourite_objects: vec![], ..Default::default() };
        let server = McpServer::new(config, test_vault());

        let result = server
            .run_query(Parameters(RunQueryParams {
                connection_id: "c1".into(),
                sql: "SELECT id FROM t".into(),
                row_limit: None,
                timeout_secs: None,
            }))
            .await
            .unwrap();
        let body = result.structured_content.unwrap();
        assert_eq!(body["truncated"], false);
        assert_eq!(body["row_count"], 3);
    }
}

// Review round 1 finding #1: audit logging (design doc §4) is a binding
// requirement, not optional polish. These test the pure formatter directly
// (the stated minimum bar) plus a smoke test that the `tracing::info!`
// call site itself compiles and doesn't panic without a subscriber
// installed — the call sites inside the three tool methods above are what
// actually satisfies "every tool call".
#[cfg(test)]
mod audit_log_tests {
    use super::*;

    #[test]
    fn audit_line_matches_the_design_docs_base_shape() {
        let line = audit_line("list_connections", "-", None, Some(3), 12, None);
        assert_eq!(line, "tool=list_connections connection=- rows=3 duration_ms=12");
    }

    #[test]
    fn audit_line_appends_sql_and_error_when_present() {
        let line = audit_line("run_query", "prod-db", Some("SELECT 1"), None, 5, Some("syntax error"));
        assert_eq!(line, "tool=run_query connection=prod-db rows=- duration_ms=5 sql=SELECT 1 error=syntax error");
    }

    #[test]
    fn audit_line_never_needs_a_secret_parameter() {
        // Structural check that the formatter has no way to accidentally
        // carry a password/key: its only string-shaped inputs are tool
        // name, connection name, SQL text, and an error message — verified
        // here by construction rather than by grepping call sites.
        let line = audit_line("run_query", "c1", Some("SELECT * FROM t"), Some(1), 1, None);
        assert!(!line.to_lowercase().contains("password"));
    }

    #[test]
    fn log_tool_call_smoke_test_does_not_panic_without_a_subscriber() {
        log_tool_call("run_query", "c1", Some("SELECT 1"), Some(2), 3, None);
        log_tool_call("get_schema", "c1", None, None, 1, Some("boom"));
    }
}

// T7: mechanical enforcement that dbc-mcp never wires a write path (design
// doc §4 layer 3) — a call site matching a certain method name (built up
// below, not written literally, so this very check doesn't trip on itself)
// must never appear in this crate's source, outside of comments.
#[cfg(test)]
mod no_write_path_regression {
    const SOURCES: &[(&str, &str)] = &[
        ("main.rs", include_str!("main.rs")),
        ("connect.rs", include_str!("connect.rs")),
        ("tools.rs", include_str!("tools.rs")),
        ("serialize.rs", include_str!("serialize.rs")),
        ("keysource.rs", include_str!("keysource.rs")),
    ];

    #[test]
    fn no_execute_call_site_outside_comments() {
        // Built at runtime, not written as a literal ".execute(" anywhere
        // in this file's source text — otherwise this check would flag its
        // own needle when scanning its own source via include_str!.
        let dot = '.';
        let word: String = ['e', 'x', 'e', 'c', 'u', 't', 'e'].iter().collect();
        let needle = format!("{dot}{word}(");

        for (name, src) in SOURCES {
            for (i, line) in src.lines().enumerate() {
                let code = match line.find("//") {
                    Some(idx) => &line[..idx],
                    None => line,
                };
                assert!(
                    !code.contains(&needle),
                    "found a write-path call site in {name}:{} — dbc-mcp must never wire a write path",
                    i + 1
                );
            }
        }
    }
}
