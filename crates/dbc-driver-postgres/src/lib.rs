mod types;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::{Field, Schema, SchemaRef};
use dbc_core::{
    quote_qualified, CancelToken, ColumnInfo, Connection, ConstraintInfo, FkRef, IndexInfo,
    QueryError, QueryStream, RoutineInfo, RoutineKind, SchemaSnapshot, SequenceInfo, TableInfo,
    TableKind, TriggerInfo, BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY,
};
use futures_util::StreamExt;
use tokio_postgres::NoTls;
use types::{arrow_type, ColBuilder};

/// Re-exported so callers outside this crate (`dbc-ui`'s `connect::open_config`)
/// can build a config for [`PostgresConnection::connect_with_config`] without
/// depending on `tokio-postgres` directly — G1 follow-up #5 (final-review.md):
/// a direct `tokio-postgres` dependency in `dbc-ui` was a protocol-crate leak
/// past the "dbc-ui sees drivers only via connect.rs dispatch" line, and a
/// version-coupling hazard (a future tokio-postgres bump here wouldn't be
/// forced to also bump in lockstep at the dbc-ui call site). `PgConfig` is
/// the exact same type as `tokio_postgres::Config`, just reachable through
/// this crate's public API instead.
pub use tokio_postgres::Config as PgConfig;

/// Every catalog query in `schema()` excludes these — internal Postgres
/// namespaces, not user objects. This also excludes session-temp namespaces
/// (`pg_temp_N` / `pg_toast_temp_N`): without this, a `CREATE TEMP TABLE`
/// issued by *any* concurrently-open session creates a real `pg_class` row
/// visible to every other backend's catalog queries for as long as that
/// session's temp schema exists, and would otherwise leak into the snapshot
/// indistinguishable from a genuine user table.
const SCHEMA_EXCLUDE: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema') \
     AND n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%' \
     AND n.nspname NOT LIKE 'pg\\_toast%'";

pub struct PostgresConnection {
    client: Arc<tokio_postgres::Client>,
}

fn pg_err(e: tokio_postgres::Error) -> QueryError {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code().to_string();
        QueryError {
            message: db.message().to_string(),
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            },
            code: Some(if code == "57014" { "cancelled".into() } else { code }),
        }
    } else {
        QueryError::msg(e.to_string())
    }
}

impl PostgresConnection {
    pub async fn connect(url: &str) -> Result<Self, QueryError> {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await.map_err(pg_err)?;
        // The connection object drives the socket; it must be polled.
        tokio::spawn(async move {
            let _ = connection.await; // errors surface on the client side
        });
        Ok(Self { client: Arc::new(client) })
    }

    /// Like [`Self::connect`], but takes a `tokio_postgres::Config` builder
    /// instead of a URL string. Used by the saved-connection path
    /// (`dbc-ui`'s `connect::open_config`) so a password containing `@`,
    /// `/`, or other URL-special characters never has to be percent-encoded
    /// into a connection string in the first place.
    pub async fn connect_with_config(config: tokio_postgres::Config) -> Result<Self, QueryError> {
        let (client, connection) = config.connect(NoTls).await.map_err(pg_err)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self { client: Arc::new(client) })
    }
}

#[async_trait]
impl Connection for PostgresConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        // prepare() gives us column names AND types before the first row,
        // and resolves as soon as Parse/Describe come back — independent of
        // how long the query itself takes to run.
        let stmt = self.client.prepare(sql).await.map_err(pg_err)?;
        let fields: Vec<Field> = stmt
            .columns()
            .iter()
            .map(|c| Field::new(c.name(), arrow_type(c.type_()), true))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let col_types: Vec<tokio_postgres::types::Type> =
            stmt.columns().iter().map(|c| c.type_().clone()).collect();

        // Protocol-level cancel goes over a separate connection. The watcher
        // must not outlive the query: once it's done (normally, on error, or
        // because the consumer dropped the stream), a `done_tx` drop races
        // against `cancelled()` so the watcher task exits either way. Without
        // this, the watcher lives until the CancelToken is cancelled (often
        // never), and a *late* cancel() — fired well after this query
        // finished — would still send a CancelRequest carrying this
        // connection's backend process id, potentially killing an unrelated
        // query that's since started using the same connection.
        let cancel_handle = self.client.cancel_token();
        let watcher_cancel = cancel.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = watcher_cancel.cancelled() => {
                    let _ = cancel_handle.cancel_query(NoTls).await;
                }
                _ = done_rx => {
                    // Query already finished; nothing to cancel.
                }
            }
        });

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let batch_schema = schema.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            // Keep `done_tx` alive for exactly this task's lifetime; its
            // drop (on any exit path below) wakes the watcher above.
            let _done_tx = done_tx;
            // IMPORTANT: query_raw() itself is awaited *inside* this task,
            // not in the `query()` method above. Bind/Execute/Sync are sent
            // as a single batch, and Postgres only flushes its response
            // buffer (BindComplete/DataRow/.../ReadyForQuery) once it has
            // processed Sync — which happens only after Execute finishes
            // running the query server-side. So query_raw().await blocks
            // until the query has essentially completed, not just until
            // streaming starts. Awaiting it here (rather than in `query()`)
            // keeps `query()` non-blocking, so the caller gets the
            // QueryStream back immediately, can react to the header right
            // away, and can still cancel a long-running query — cancelling
            // before this await resolves makes it fail with a "cancelled"
            // QueryError instead of hanging for the query's full duration.
            let params: Vec<String> = Vec::new();
            let row_stream = match client.query_raw(&stmt, params).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(pg_err(e))).await;
                    return;
                }
            };
            // RowStream is !Unpin (contains PhantomPinned); pin it in this
            // task's stack frame so StreamExt::next() can be called on it.
            tokio::pin!(row_stream);
            let new_builders = |types: &[tokio_postgres::types::Type]| -> Vec<ColBuilder> {
                types.iter().map(ColBuilder::for_type).collect()
            };
            let mut builders = new_builders(&col_types);
            let mut in_batch = 0usize;
            let mut deadline: Option<tokio::time::Instant> = None;

            loop {
                let next = if let Some(d) = deadline {
                    tokio::select! {
                        r = row_stream.next() => Some(r),
                        _ = tokio::time::sleep_until(d) => None, // latency flush
                    }
                } else {
                    Some(row_stream.next().await)
                };

                let flush_now = match next {
                    None => true, // 16ms deadline hit
                    Some(None) => { // stream done
                        if in_batch > 0 {
                            let arrays = builders.iter_mut().map(|b| b.finish()).collect();
                            if let Ok(b) = RecordBatch::try_new(batch_schema.clone(), arrays) {
                                let _ = tx.send(Ok(b)).await;
                            }
                        }
                        break;
                    }
                    Some(Some(Err(e))) => {
                        let _ = tx.send(Err(pg_err(e))).await;
                        break;
                    }
                    Some(Some(Ok(row))) => {
                        for (i, b) in builders.iter_mut().enumerate() {
                            b.append(&row, i);
                        }
                        in_batch += 1;
                        if in_batch == 1 {
                            deadline = Some(tokio::time::Instant::now() + BATCH_LATENCY);
                        }
                        in_batch >= BATCH_ROWS
                    }
                };

                if flush_now && in_batch > 0 {
                    let arrays = builders.iter_mut().map(|b| b.finish()).collect();
                    match RecordBatch::try_new(batch_schema.clone(), arrays) {
                        Ok(b) => {
                            if tx.send(Ok(b)).await.is_err() { break; } // consumer gone
                        }
                        Err(e) => {
                            let _ = tx.send(Err(QueryError::msg(e.to_string()))).await;
                            break;
                        }
                    }
                    builders = new_builders(&col_types);
                    in_batch = 0;
                    deadline = None;
                }
            }
        });

        Ok(QueryStream { columns: schema, batches: rx })
    }

    /// Executes a non-returning statement over `self.client` (an `Arc`
    /// shared for the lifetime of this `PostgresConnection`, i.e. one
    /// backend session), so `BEGIN … COMMIT`/`ROLLBACK` issued via successive
    /// `execute` calls run within the same server-side transaction.
    ///
    /// T4 review round 1, MAJOR 2 (part b): gives `execute()` the same
    /// protocol-level cancel watcher `query()` already has (see that
    /// method's doc comment for the full "must not outlive the query"
    /// reasoning) — this is what lets `dbc-ui`'s
    /// `run_write_transaction_bounded` actually reach the backend when its
    /// outer timeout fires mid-statement, instead of merely dropping the
    /// Rust-side future while the statement keeps running server-side.
    ///
    /// Unlike `query()`, there is no separate streaming task to naturally
    /// hang the watcher's lifetime off of — so the ACTUAL `client.execute`
    /// call is ALSO run in an independently `tokio::spawn`ed task here (not
    /// simply awaited inline in this fn's own stack frame), with the
    /// watcher's `done_tx` moved into THAT task. This detachment is the
    /// whole point: a caller that drops the future THIS `execute()` call
    /// returns (e.g. `dbc-ui`'s `tokio::time::timeout` firing) must not also
    /// silently kill the watcher — if `done_tx` instead lived in this fn's
    /// own async-generator frame, dropping this future would drop `done_tx`
    /// too, and the watcher's `tokio::select!` would take its "done, nothing
    /// to cancel" branch before a subsequent explicit `cancel.cancel()` call
    /// ever has a chance to run, losing the race every time. Spawning the
    /// actual execution keeps it — and the watcher — alive regardless of
    /// whether the CALLER is still awaiting this returned future.
    async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError> {
        if cancel.is_cancelled() {
            return Err(QueryError {
                code: Some("cancelled".into()),
                message: "query cancelled".into(),
                position: None,
            });
        }

        let cancel_handle = self.client.cancel_token();
        let watcher_cancel = cancel.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = watcher_cancel.cancelled() => {
                    let _ = cancel_handle.cancel_query(NoTls).await;
                }
                _ = done_rx => {
                    // Statement already finished; nothing to cancel.
                }
            }
        });

        let client = self.client.clone();
        let sql = sql.to_owned();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            // Keep `done_tx` alive for exactly this DETACHED task's
            // lifetime — see this method's doc comment for why it must NOT
            // live in the outer (droppable) fn frame instead.
            let _done_tx = done_tx;
            let result = client.execute(&sql, &[]).await.map_err(pg_err);
            // Receiver may already be gone if the caller dropped this
            // `execute()` call's future (e.g. on timeout) — that's fine,
            // there's simply nothing left to deliver the result to.
            let _ = result_tx.send(result);
        });

        result_rx.await.map_err(|_| QueryError::msg("driver task died"))?
    }

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let (mut tables, oid_idx, col_idx) = self.fetch_tables().await?;
        self.attach_pks(&mut tables, &col_idx).await?;
        self.attach_fks(&mut tables, &col_idx).await?;
        self.attach_constraints(&mut tables, &oid_idx).await?;
        self.attach_indexes(&mut tables, &oid_idx).await?;
        self.attach_view_ddl(&mut tables, &oid_idx).await?;
        let routines = self.fetch_routines().await?;
        let triggers = self.fetch_triggers().await?;
        let sequences = self.fetch_sequences().await?;
        Ok(SchemaSnapshot { tables, routines, triggers, sequences })
    }
}

/// Type alias for the (table_oid, column_name) -> (table index, column
/// index) lookup threaded through the catalog-fetch helpers below: it lets
/// per-row PK/FK/index results (keyed by oid + column name, straight off
/// pg_attribute) find the `ColumnInfo` they belong to without a linear scan.
type ColLookup = HashMap<(u32, String), (usize, usize)>;

impl PostgresConnection {
    /// Tables, views and materialized views with their columns, in one pass:
    /// `pg_class` joined to `pg_namespace` (schema) and `pg_attribute`
    /// (columns, skipping dropped/system columns), with `pg_attrdef` for
    /// column defaults. Also builds the oid/column lookup tables the other
    /// `attach_*` helpers use to fill in the rest of `TableInfo`.
    async fn fetch_tables(
        &self,
    ) -> Result<(Vec<TableInfo>, HashMap<u32, usize>, ColLookup), QueryError> {
        let sql = format!(
            "SELECT c.oid, n.nspname, c.relname, c.relkind::text, a.attname,
                    format_type(a.atttypid, a.atttypmod) AS data_type,
                    a.attnotnull, pg_get_expr(ad.adbin, ad.adrelid) AS default_expr
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_attribute a ON a.attrelid = c.oid
             LEFT JOIN pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum
             WHERE c.relkind IN ('r', 'v', 'm')
               AND a.attnum > 0 AND NOT a.attisdropped
               AND {SCHEMA_EXCLUDE}
             ORDER BY n.nspname, c.relname, a.attnum"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;

        let mut tables: Vec<TableInfo> = Vec::new();
        let mut oid_idx: HashMap<u32, usize> = HashMap::new();
        let mut col_idx: ColLookup = HashMap::new();

        for row in rows {
            let oid: u32 = row.try_get(0).map_err(pg_err)?;
            let schema: String = row.try_get(1).map_err(pg_err)?;
            let name: String = row.try_get(2).map_err(pg_err)?;
            let relkind: String = row.try_get(3).map_err(pg_err)?;
            let col_name: String = row.try_get(4).map_err(pg_err)?;
            let data_type: String = row.try_get(5).map_err(pg_err)?;
            let attnotnull: bool = row.try_get(6).map_err(pg_err)?;
            let default: Option<String> = row.try_get(7).map_err(pg_err)?;

            let kind = match relkind.as_str() {
                "v" => TableKind::View,
                "m" => TableKind::MaterializedView,
                _ => TableKind::Table,
            };

            let table_idx = *oid_idx.entry(oid).or_insert_with(|| {
                tables.push(TableInfo {
                    schema: Some(schema.clone()),
                    name: name.clone(),
                    kind,
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    ddl: None,
                });
                tables.len() - 1
            });

            let table = &mut tables[table_idx];
            let col_pos = table.columns.len();
            table.columns.push(ColumnInfo {
                name: col_name.clone(),
                data_type,
                nullable: !attnotnull,
                default,
                is_pk: false,
                fk: None,
            });
            col_idx.insert((oid, col_name), (table_idx, col_pos));
        }

        Ok((tables, oid_idx, col_idx))
    }

    /// PKs: `pg_index` where `indisprimary`, `indkey` (cast from
    /// `int2vector` to a real `int2[]` so it can be unnested) joined back to
    /// `pg_attribute` for column names.
    async fn attach_pks(
        &self,
        tables: &mut [TableInfo],
        col_idx: &ColLookup,
    ) -> Result<(), QueryError> {
        let sql = format!(
            "SELECT i.indrelid, a.attname
             FROM pg_index i
             JOIN pg_class c ON c.oid = i.indrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN LATERAL unnest(i.indkey::int2[]) AS k(attnum) ON true
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
             WHERE i.indisprimary AND {SCHEMA_EXCLUDE}"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        for row in rows {
            let oid: u32 = row.try_get(0).map_err(pg_err)?;
            let col_name: String = row.try_get(1).map_err(pg_err)?;
            if let Some(&(t_idx, c_idx)) = col_idx.get(&(oid, col_name)) {
                tables[t_idx].columns[c_idx].is_pk = true;
            }
        }
        Ok(())
    }

    /// FKs: `pg_constraint` with `contype = 'f'`, `conkey`/`confkey` zipped
    /// pairwise via multi-argument `unnest` (Postgres unnests parallel
    /// arrays by position when given more than one array argument), each
    /// pair resolved to a local column name (`conrelid`/`conkey`) and a
    /// target schema/table/column (`confrelid`/`confkey`).
    async fn attach_fks(
        &self,
        tables: &mut [TableInfo],
        col_idx: &ColLookup,
    ) -> Result<(), QueryError> {
        let sql = format!(
            "SELECT con.conrelid, a.attname, fn.nspname, fc.relname, fa.attname
             FROM pg_constraint con
             JOIN pg_namespace n ON n.oid = con.connamespace
             JOIN pg_class fc ON fc.oid = con.confrelid
             JOIN pg_namespace fn ON fn.oid = fc.relnamespace
             JOIN LATERAL unnest(con.conkey, con.confkey) AS cols(lk, fk) ON true
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = cols.lk
             JOIN pg_attribute fa ON fa.attrelid = con.confrelid AND fa.attnum = cols.fk
             WHERE con.contype = 'f' AND {SCHEMA_EXCLUDE}"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        for row in rows {
            let oid: u32 = row.try_get(0).map_err(pg_err)?;
            let col_name: String = row.try_get(1).map_err(pg_err)?;
            let f_schema: String = row.try_get(2).map_err(pg_err)?;
            let f_table: String = row.try_get(3).map_err(pg_err)?;
            let f_col: String = row.try_get(4).map_err(pg_err)?;
            if let Some(&(t_idx, c_idx)) = col_idx.get(&(oid, col_name)) {
                tables[t_idx].columns[c_idx].fk =
                    Some(FkRef { schema: Some(f_schema), table: f_table, column: f_col });
            }
        }
        Ok(())
    }

    /// All constraints (not just FKs/PKs) on each table: name, kind mapped
    /// from `contype`, and the human-readable body from
    /// `pg_get_constraintdef`.
    async fn attach_constraints(
        &self,
        tables: &mut [TableInfo],
        oid_idx: &HashMap<u32, usize>,
    ) -> Result<(), QueryError> {
        let sql = format!(
            "SELECT con.conrelid, con.conname, con.contype::text, pg_get_constraintdef(con.oid)
             FROM pg_constraint con
             JOIN pg_class c ON c.oid = con.conrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE {SCHEMA_EXCLUDE}
             ORDER BY n.nspname, c.relname, con.conname"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        for row in rows {
            let oid: u32 = row.try_get(0).map_err(pg_err)?;
            let name: String = row.try_get(1).map_err(pg_err)?;
            let contype: String = row.try_get(2).map_err(pg_err)?;
            let definition: String = row.try_get(3).map_err(pg_err)?;
            let kind = match contype.as_str() {
                "p" => "PRIMARY KEY",
                "f" => "FOREIGN KEY",
                "u" => "UNIQUE",
                "c" => "CHECK",
                other => other,
            }
            .to_string();
            if let Some(&t_idx) = oid_idx.get(&oid) {
                tables[t_idx].constraints.push(ConstraintInfo { name, kind, definition });
            }
        }
        Ok(())
    }

    /// Non-PK indexes: `pg_index` joined to `pg_class` for the index name,
    /// `indisunique`, and column names in index order via `indkey` unnested
    /// `WITH ORDINALITY` and aggregated back into an ordered `Vec<String>`.
    /// PK-backing indexes are skipped — they're already represented via
    /// `ColumnInfo::is_pk` and `ConstraintInfo`.
    ///
    /// Expression/functional index columns have `indkey` entries of `0`
    /// (there is no backing `pg_attribute` row for attnum 0), so the join to
    /// `pg_attribute` is a LEFT JOIN and, when it doesn't match (`ord.attnum
    /// = 0`), the expression text is rendered instead via
    /// `pg_get_indexdef(indexrelid, position, true)` — this keeps a mixed
    /// index like `(id, lower(body))` reporting both columns, in order,
    /// rather than silently dropping the expression one.
    async fn attach_indexes(
        &self,
        tables: &mut [TableInfo],
        oid_idx: &HashMap<u32, usize>,
    ) -> Result<(), QueryError> {
        let sql = format!(
            "SELECT i.indrelid, ic.relname, i.indisunique,
                    array_agg(
                        CASE WHEN ord.attnum = 0
                             THEN pg_get_indexdef(i.indexrelid, ord.n::int, true)
                             ELSE a.attname
                        END ORDER BY ord.n
                    ) AS cols
             FROM pg_index i
             JOIN pg_class ic ON ic.oid = i.indexrelid
             JOIN pg_class c ON c.oid = i.indrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS ord(attnum, n) ON true
             LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ord.attnum
             WHERE NOT i.indisprimary AND {SCHEMA_EXCLUDE}
             GROUP BY i.indrelid, i.indexrelid, ic.relname, i.indisunique, n.nspname, c.relname
             ORDER BY n.nspname, c.relname, ic.relname"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        for row in rows {
            let oid: u32 = row.try_get(0).map_err(pg_err)?;
            let name: String = row.try_get(1).map_err(pg_err)?;
            let unique: bool = row.try_get(2).map_err(pg_err)?;
            let columns: Vec<String> = row.try_get(3).map_err(pg_err)?;
            if let Some(&t_idx) = oid_idx.get(&oid) {
                tables[t_idx].indexes.push(IndexInfo { name, columns, unique });
            }
        }
        Ok(())
    }

    /// View/matview DDL: `pg_get_viewdef(oid, true)` wrapped in
    /// `CREATE [MATERIALIZED ]VIEW ... AS`. Plain tables keep `ddl: None` —
    /// Postgres has no server-side "get table DDL", so the UI synthesizes it
    /// via `dbc_core::ddl::synthesize_create_table`.
    async fn attach_view_ddl(
        &self,
        tables: &mut [TableInfo],
        oid_idx: &HashMap<u32, usize>,
    ) -> Result<(), QueryError> {
        let sql = format!(
            "SELECT c.oid, n.nspname, c.relname, c.relkind::text, pg_get_viewdef(c.oid, true)
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('v', 'm') AND {SCHEMA_EXCLUDE}"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        for row in rows {
            let oid: u32 = row.try_get(0).map_err(pg_err)?;
            let schema: String = row.try_get(1).map_err(pg_err)?;
            let name: String = row.try_get(2).map_err(pg_err)?;
            let relkind: String = row.try_get(3).map_err(pg_err)?;
            let def: String = row.try_get(4).map_err(pg_err)?;
            if let Some(&t_idx) = oid_idx.get(&oid) {
                let kw = if relkind == "m" { "MATERIALIZED VIEW" } else { "VIEW" };
                let qual = quote_qualified(Some(&schema), &name);
                tables[t_idx].ddl = Some(format!("CREATE {kw} {qual} AS\n{def}"));
            }
        }
        Ok(())
    }

    /// Routines: `pg_proc` filtered to `prokind IN ('f', 'p')` (functions and
    /// procedures — aggregates 'a' and window functions 'w' are excluded up
    /// front by the WHERE clause). Signature comes from
    /// `pg_get_function_arguments` plus, for functions only,
    /// `pg_get_function_result`. `pg_get_functiondef` can still fail per-row
    /// for routines it can't reconstruct; that failure is caught per row
    /// (ddl becomes `None`) rather than failing the whole snapshot.
    async fn fetch_routines(&self) -> Result<Vec<RoutineInfo>, QueryError> {
        let sql = format!(
            "SELECT n.nspname, p.proname, p.prokind::text, p.oid,
                    pg_get_function_arguments(p.oid) AS args,
                    pg_get_function_result(p.oid) AS result
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE p.prokind IN ('f', 'p') AND {SCHEMA_EXCLUDE}
             ORDER BY n.nspname, p.proname"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;

        let mut routines = Vec::with_capacity(rows.len());
        for row in rows {
            let schema: String = row.try_get(0).map_err(pg_err)?;
            let name: String = row.try_get(1).map_err(pg_err)?;
            let prokind: String = row.try_get(2).map_err(pg_err)?;
            let oid: u32 = row.try_get(3).map_err(pg_err)?;
            let args: String = row.try_get(4).map_err(pg_err)?;
            let result: Option<String> = row.try_get(5).map_err(pg_err)?;

            let kind =
                if prokind == "p" { RoutineKind::Procedure } else { RoutineKind::Function };
            let signature = match (kind, &result) {
                (RoutineKind::Function, Some(r)) => format!("({args}) -> {r}"),
                _ => format!("({args})"),
            };

            let ddl = match self
                .client
                .query_one("SELECT pg_get_functiondef($1::oid)", &[&oid])
                .await
            {
                Ok(r) => r.try_get::<_, Option<String>>(0).unwrap_or(None),
                Err(_) => None, // tolerated: see doc comment above
            };

            routines.push(RoutineInfo { schema: Some(schema), name, kind, signature, ddl });
        }
        Ok(routines)
    }

    /// Triggers: `pg_trigger` excluding internal ones (`tgisinternal` —
    /// backing triggers for FK enforcement, not user-authored), with
    /// `pg_get_triggerdef` for the DDL.
    async fn fetch_triggers(&self) -> Result<Vec<TriggerInfo>, QueryError> {
        let sql = format!(
            "SELECT n.nspname, t.tgname, c.relname, pg_get_triggerdef(t.oid)
             FROM pg_trigger t
             JOIN pg_class c ON c.oid = t.tgrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE NOT t.tgisinternal AND {SCHEMA_EXCLUDE}
             ORDER BY n.nspname, c.relname, t.tgname"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        let mut triggers = Vec::with_capacity(rows.len());
        for row in rows {
            let schema: String = row.try_get(0).map_err(pg_err)?;
            let name: String = row.try_get(1).map_err(pg_err)?;
            let table: String = row.try_get(2).map_err(pg_err)?;
            let def: String = row.try_get(3).map_err(pg_err)?;
            triggers.push(TriggerInfo { schema: Some(schema), name, table, ddl: Some(def) });
        }
        Ok(triggers)
    }

    /// Sequences: `pg_class` with `relkind = 'S'`.
    async fn fetch_sequences(&self) -> Result<Vec<SequenceInfo>, QueryError> {
        let sql = format!(
            "SELECT n.nspname, c.relname
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind = 'S' AND {SCHEMA_EXCLUDE}
             ORDER BY n.nspname, c.relname"
        );
        let rows = self.client.query(&sql, &[]).await.map_err(pg_err)?;
        let mut sequences = Vec::with_capacity(rows.len());
        for row in rows {
            let schema: String = row.try_get(0).map_err(pg_err)?;
            let name: String = row.try_get(1).map_err(pg_err)?;
            sequences.push(SequenceInfo { schema: Some(schema), name });
        }
        Ok(sequences)
    }
}
