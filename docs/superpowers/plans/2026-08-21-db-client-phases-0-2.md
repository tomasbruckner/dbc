# DB Client Phases 0–2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A GPUI desktop app on Windows that streams query results from SQLite and PostgreSQL into a virtualized million-row grid, with first-rows-fast rendering and protocol-level cancellation.

**Architecture:** Cargo workspace with strict layering: `dbc-core` (traits + Arrow types, knows nothing of UI or drivers) ← `dbc-driver-sqlite` / `dbc-driver-postgres` ← `dbc-ui` (GPUI binary). Results flow as Arrow `RecordBatch`es over bounded tokio channels; `dbc-buffer` stores them with prefix-sum row indexing and disk spill.

**Tech Stack:** Rust stable (MSVC), GPUI (git-pinned to zed-industries/zed), tokio, arrow 59, rusqlite (bundled), tokio-postgres, testcontainers, criterion.

**Spec:** `docs/superpowers/specs/2026-08-21-db-client-phases-0-2-design.md`

## Global Constraints

- GPUI is pinned by git rev: `gpui = { git = "https://github.com/zed-industries/zed", rev = "907ed09c9f4476caf250e6ce4bbffb23b4622f3b" }` (and same for `gpui_platform`). Never `cargo update` this dependency; upgrades are their own commits.
- `dbc-core` MUST NOT depend on gpui or any driver crate. `dbc-ui` MUST NOT depend on rusqlite/tokio-postgres directly (only via driver crates).
- No `.await` on the UI thread except inside `cx.spawn` futures; no GPUI calls off the UI thread.
- Batch limits (from spec, used in two drivers): `BATCH_ROWS = 1024`, `BATCH_LATENCY = 16 ms`, channel capacity `8`.
- Buffer memory cap default: `500_000` rows; excess spills to disk as Arrow IPC.
- Errors are values: `QueryError { code, message, position }`. Never `panic!` on a DB error path; never log connection strings.
- Workspace crates use `edition = "2021"`. All crates named `dbc-*`.
- Integration tests requiring Docker are marked `#[ignore]`.
- Commit after every task (at minimum). Commit messages: conventional style (`feat:`, `test:`, `chore:`).

---

### Task 1: Toolchain + workspace skeleton (Phase 0)

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `crates/dbc-core/Cargo.toml`, `crates/dbc-core/src/lib.rs` (empty lib)

**Interfaces:**
- Consumes: nothing.
- Produces: a building workspace; later tasks add crates to `members`.

- [ ] **Step 1: Install Rust toolchain (Windows/MSVC)**

Run in PowerShell:
```powershell
winget install --id Rustlang.Rustup -e --accept-package-agreements --accept-source-agreements
```
Then check for the MSVC linker (GPUI needs the C++ build tools):
```powershell
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```
Open a NEW shell afterwards (PATH changes), then:
```powershell
rustup default stable
rustc --version; cargo --version
```
Expected: both print versions (rustc 1.8x+). If `link.exe` errors appear later, the Build Tools install did not finish — re-run it.

- [ ] **Step 2: Create workspace files**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/dbc-core"]

[workspace.package]
edition = "2021"

[workspace.dependencies]
anyhow = "1"
arrow = "59"
async-trait = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
tokio-util = "0.7"
```

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "stable"
```

`.gitignore`:
```
/target
```

`crates/dbc-core/Cargo.toml`:
```toml
[package]
name = "dbc-core"
version = "0.1.0"
edition.workspace = true

[dependencies]
```

`crates/dbc-core/src/lib.rs`:
```rust
// Core types and traits. No UI, no concrete drivers — ever.
```

- [ ] **Step 3: Verify the workspace builds**

Run: `cargo build`
Expected: `Finished` with no errors.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: workspace skeleton + toolchain pin"
```

---

### Task 2: Empty GPUI window on Windows (Phase 0 verdict)

**Files:**
- Create: `crates/dbc-ui/Cargo.toml`
- Create: `crates/dbc-ui/src/main.rs`
- Modify: `Cargo.toml` (add member)

**Interfaces:**
- Consumes: nothing.
- Produces: `dbc-ui` binary; the `AppView` root view that later tasks extend.

- [ ] **Step 1: Add crate**

Add `"crates/dbc-ui"` to `members` in root `Cargo.toml`.

`crates/dbc-ui/Cargo.toml`:
```toml
[package]
name = "dbc-ui"
version = "0.1.0"
edition.workspace = true

[dependencies]
gpui = { git = "https://github.com/zed-industries/zed", rev = "907ed09c9f4476caf250e6ce4bbffb23b4622f3b" }
gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "907ed09c9f4476caf250e6ce4bbffb23b4622f3b" }
```
Note: if `gpui_platform` fails to resolve as a crate name at this rev, list crates in the Zed repo checkout under `~/.cargo/git/checkouts/zed-*/crates/` and adjust — the standalone entry point per Zed's own `crates/gpui/examples/hello_world.rs` is `gpui_platform::application()`.

- [ ] **Step 2: Minimal window (adapted from Zed's hello_world example)**

`crates/dbc-ui/src/main.rs`:
```rust
use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use gpui_platform::application;

struct AppView;

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .justify_center()
            .items_center()
            .text_color(rgb(0xcdd6f4))
            .child("dbc — phase 0")
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| AppView),
        )
        .unwrap();
        cx.activate(true);
    });
}
```

- [ ] **Step 3: Build and run (this is the bet-verdict step)**

Run: `cargo run -p dbc-ui`
Expected: first build takes many minutes (compiling the pinned Zed subtree). Then a 1200×800 window opens with dark background and the text "dbc — phase 0". Closing the window exits cleanly.
If this fails to build on Windows after honest debugging (2–3 attempts at fixing feature flags/toolchain), STOP and report — the spec names the fallback stacks; do not burn days here.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: empty GPUI window (phase 0 complete)"
```

---

### Task 3: dbc-core — errors, cancel, stream, trait (Phase 1)

**Files:**
- Modify: `crates/dbc-core/Cargo.toml`
- Create: `crates/dbc-core/src/error.rs`, `src/cancel.rs`, `src/stream.rs`, `src/connection.rs`, `src/schema.rs`
- Modify: `crates/dbc-core/src/lib.rs`
- Test: inline `#[cfg(test)]` in `error.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (all `pub use`d from `dbc_core` root — drivers and UI import from here):
  - `QueryError { code: Option<String>, message: String, position: Option<u32> }`, `QueryError::msg(impl Into<String>) -> QueryError`
  - `CancelToken` (= `tokio_util::sync::CancellationToken`)
  - `QueryStream { columns: SchemaRef, batches: tokio::sync::mpsc::Receiver<Result<RecordBatch, QueryError>> }`
  - Constants `BATCH_ROWS: usize = 1024`, `BATCH_LATENCY: Duration = 16ms`, `CHANNEL_CAPACITY: usize = 8`
  - `trait Connection: Send { async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError>; async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError>; }` (via `async_trait`)
  - `SchemaSnapshot { tables: Vec<TableInfo> }`, `TableInfo { schema: Option<String>, name: String, columns: Vec<ColumnInfo> }`, `ColumnInfo { name: String, data_type: String }`

- [ ] **Step 1: Dependencies**

`crates/dbc-core/Cargo.toml` `[dependencies]`:
```toml
arrow.workspace = true
async-trait.workspace = true
tokio.workspace = true
tokio-util.workspace = true
```

- [ ] **Step 2: Write failing test**

In `crates/dbc-core/src/error.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_includes_code_and_position() {
        let e = QueryError { code: Some("42601".into()), message: "syntax error".into(), position: Some(15) };
        assert_eq!(e.to_string(), "[42601] syntax error (at 15)");
        assert_eq!(QueryError::msg("boom").to_string(), "boom");
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p dbc-core`
Expected: compile error — `QueryError` not defined.

- [ ] **Step 4: Implement**

`error.rs`:
```rust
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub code: Option<String>,
    pub message: String,
    /// Byte offset into the SQL text, when the server reports one.
    pub position: Option<u32>,
}

impl QueryError {
    pub fn msg(m: impl Into<String>) -> Self {
        Self { code: None, message: m.into(), position: None }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(c) = &self.code { write!(f, "[{c}] ")?; }
        write!(f, "{}", self.message)?;
        if let Some(p) = self.position { write!(f, " (at {p})")?; }
        Ok(())
    }
}

impl std::error::Error for QueryError {}
```

`cancel.rs`:
```rust
/// Cooperative cancellation. Drivers watch this token and issue a
/// protocol-level cancel (pg CancelRequest / sqlite interrupt) when fired.
pub use tokio_util::sync::CancellationToken as CancelToken;
```

`stream.rs`:
```rust
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
pub struct QueryStream {
    pub columns: SchemaRef,
    pub batches: tokio::sync::mpsc::Receiver<Result<RecordBatch, QueryError>>,
}
```

`schema.rs`:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSnapshot { pub tables: Vec<TableInfo> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo { pub name: String, pub data_type: String }
```

`connection.rs`:
```rust
use async_trait::async_trait;
use crate::{cancel::CancelToken, error::QueryError, schema::SchemaSnapshot, stream::QueryStream};

#[async_trait]
pub trait Connection: Send {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError>;
    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError>;
}
```

`lib.rs`:
```rust
mod cancel;
mod connection;
mod error;
mod schema;
mod stream;

pub use cancel::CancelToken;
pub use connection::Connection;
pub use error::QueryError;
pub use schema::{ColumnInfo, SchemaSnapshot, TableInfo};
pub use stream::{QueryStream, BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY};

// Re-export so drivers/UI use one arrow version.
pub use arrow;
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p dbc-core`
Expected: PASS (1 test).

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: dbc-core trait, QueryStream, QueryError"
```

---

### Task 4: dbc-buffer — prefix-sum result storage (Phase 1)

**Files:**
- Create: `crates/dbc-buffer/Cargo.toml`, `crates/dbc-buffer/src/lib.rs`
- Modify: root `Cargo.toml` members

**Interfaces:**
- Consumes: `dbc_core::arrow` (RecordBatch, SchemaRef).
- Produces:
  - `ResultBuffer::new(schema: SchemaRef) -> ResultBuffer`
  - `ResultBuffer::push(&mut self, batch: RecordBatch)`
  - `ResultBuffer::row_count(&self) -> usize`
  - `ResultBuffer::column_count(&self) -> usize`
  - `ResultBuffer::cell_text(&mut self, row: usize, col: usize) -> String` (empty string for null; `&mut` because spill adds a read cache later)
  - `ResultBuffer::schema(&self) -> &SchemaRef`

- [ ] **Step 1: Crate setup**

Add `"crates/dbc-buffer"` to members. `crates/dbc-buffer/Cargo.toml`:
```toml
[package]
name = "dbc-buffer"
version = "0.1.0"
edition.workspace = true

[dependencies]
dbc-core = { path = "../dbc-core" }
```

- [ ] **Step 2: Write failing tests**

In `crates/dbc-buffer/src/lib.rs` (tests first, at bottom):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::arrow::array::{Int64Array, StringArray, RecordBatch};
    use dbc_core::arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    fn batch(start: i64, n: usize) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let ids = Int64Array::from_iter_values(start..start + n as i64);
        let names = StringArray::from_iter((0..n).map(|i| {
            if i % 7 == 0 { None } else { Some(format!("row{}", start + i as i64)) }
        }));
        RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(names)]).unwrap()
    }

    #[test]
    fn indexes_across_batches() {
        let b0 = batch(0, 100);
        let mut buf = ResultBuffer::new(b0.schema());
        buf.push(b0);
        buf.push(batch(100, 50));
        buf.push(batch(150, 25));
        assert_eq!(buf.row_count(), 175);
        assert_eq!(buf.column_count(), 2);
        assert_eq!(buf.cell_text(0, 0), "0");
        assert_eq!(buf.cell_text(99, 0), "99");   // last row of batch 0
        assert_eq!(buf.cell_text(100, 0), "100"); // first row of batch 1
        assert_eq!(buf.cell_text(174, 0), "174");
        assert_eq!(buf.cell_text(1, 1), "row1");
    }

    #[test]
    fn null_renders_empty() {
        let b = batch(0, 10);
        let mut buf = ResultBuffer::new(b.schema());
        buf.push(b);
        assert_eq!(buf.cell_text(0, 1), ""); // i % 7 == 0 → None
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p dbc-buffer`
Expected: compile error — `ResultBuffer` not defined.

- [ ] **Step 4: Implement**

Top of `crates/dbc-buffer/src/lib.rs`:
```rust
use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::arrow::util::display::array_value_to_string;

/// Columnar result storage. `offsets[i]` = number of rows in batches [0, i),
/// so locating a row is a binary search — O(log n) per lookup, O(1) push.
pub struct ResultBuffer {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    offsets: Vec<usize>, // len == batches.len() + 1; offsets[0] == 0
}

impl ResultBuffer {
    pub fn new(schema: SchemaRef) -> Self {
        Self { schema, batches: Vec::new(), offsets: vec![0] }
    }

    pub fn push(&mut self, batch: RecordBatch) {
        let total = self.offsets.last().copied().unwrap_or(0) + batch.num_rows();
        self.offsets.push(total);
        self.batches.push(batch);
    }

    pub fn row_count(&self) -> usize {
        *self.offsets.last().unwrap()
    }

    pub fn column_count(&self) -> usize {
        self.schema.fields().len()
    }

    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// (batch index, row-within-batch) for an absolute row.
    fn locate(&self, row: usize) -> (usize, usize) {
        let bi = self.offsets.partition_point(|&off| off <= row) - 1;
        (bi, row - self.offsets[bi])
    }

    pub fn cell_text(&mut self, row: usize, col: usize) -> String {
        if row >= self.row_count() || col >= self.column_count() {
            return String::new();
        }
        let (bi, ri) = self.locate(row);
        let array = self.batches[bi].column(col);
        if array.is_null(ri) {
            return String::new();
        }
        array_value_to_string(array, ri).unwrap_or_default()
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p dbc-buffer`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: dbc-buffer with prefix-sum row indexing"
```

---

### Task 5: dbc-driver-sqlite (Phase 1)

**Files:**
- Create: `crates/dbc-driver-sqlite/Cargo.toml`, `src/lib.rs`
- Modify: root `Cargo.toml` members

**Interfaces:**
- Consumes: `dbc_core::{Connection, QueryStream, QueryError, CancelToken, BATCH_ROWS, CHANNEL_CAPACITY, SchemaSnapshot, TableInfo, ColumnInfo}`.
- Produces: `SqliteConnection::new(path: impl Into<PathBuf>) -> SqliteConnection`, implementing `dbc_core::Connection`.
- Design notes an engineer needs:
  - SQLite is dynamically typed → every column is Arrow `Utf8` (nullable), values formatted to text. Postgres (Task 7) does real typed arrays; SQLite is the phase-1 vehicle.
  - Each `query()` opens its own `rusqlite::Connection` inside `spawn_blocking` (SQLite open is cheap; keeps the blocking work off tokio workers and sidesteps `!Sync`).
  - Cancel: obtain `conn.get_interrupt_handle()` on the blocking thread, hand it to a watcher task that calls `.interrupt()` when the token fires. Interrupted queries surface as `QueryError { code: Some("cancelled") }`.

- [ ] **Step 1: Crate setup**

Add `"crates/dbc-driver-sqlite"` to members. `Cargo.toml`:
```toml
[package]
name = "dbc-driver-sqlite"
version = "0.1.0"
edition.workspace = true

[dependencies]
dbc-core = { path = "../dbc-core" }
rusqlite = { version = "0.40", features = ["bundled"] }
tokio.workspace = true
async-trait.workspace = true

[dev-dependencies]
tokio = { version = "1", features = ["full"] }
```

- [ ] **Step 2: Write failing tests**

Bottom of `src/lib.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{CancelToken, Connection};

    fn fixture_db() -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(f.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE t(id INTEGER, name TEXT);
             INSERT INTO t SELECT value, 'n' || value FROM generate_series(1, 5000);",
        ).unwrap();
        f
    }

    #[tokio::test]
    async fn streams_all_rows_in_batches() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        let mut s = c.query("SELECT id, name FROM t ORDER BY id", CancelToken::new()).await.unwrap();
        assert_eq!(s.columns.fields().len(), 2);
        assert_eq!(s.columns.field(0).name(), "id");
        let mut rows = 0usize;
        let mut batches = 0usize;
        while let Some(b) = s.batches.recv().await {
            let b = b.unwrap();
            rows += b.num_rows();
            batches += 1;
        }
        assert_eq!(rows, 5000);
        assert!(batches >= 4, "expected multiple 1024-row batches, got {batches}");
    }

    #[tokio::test]
    async fn sql_error_is_a_value() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        let err = c.query("SELECT * FROM missing_table", CancelToken::new()).await.unwrap_err();
        assert!(err.message.contains("missing_table"));
    }

    #[tokio::test]
    async fn cancel_interrupts_long_query() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        let cancel = CancelToken::new();
        // Cross join = 25M rows; must not complete before interrupt.
        let mut s = c
            .query("SELECT a.id FROM t a, t b", cancel.clone())
            .await
            .unwrap();
        cancel.cancel();
        let mut saw_cancel = false;
        while let Some(r) = s.batches.recv().await {
            if let Err(e) = r {
                assert_eq!(e.code.as_deref(), Some("cancelled"));
                saw_cancel = true;
            }
        }
        assert!(saw_cancel, "stream ended without a cancelled error");
    }

    #[tokio::test]
    async fn schema_lists_tables_and_columns() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        let snap = c.schema().await.unwrap();
        let t = snap.tables.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[0].name, "id");
    }
}
```
Add `tempfile = "3"` to `[dev-dependencies]`. Note: `generate_series` needs SQLite ≥ 3.34 with the series extension — rusqlite `bundled` includes it via `SQLITE_ENABLE_MATH_FUNCTIONS`? It does NOT reliably. If `generate_series` is unavailable, replace fixture insert with a plain Rust loop of 5000 `INSERT`s inside one transaction — do that adjustment silently if the first test run errors on it.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p dbc-driver-sqlite`
Expected: compile error — `SqliteConnection` not defined.

- [ ] **Step 4: Implement**

`src/lib.rs` top:
```rust
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, QueryError, QueryStream, SchemaSnapshot, TableInfo,
    BATCH_ROWS, CHANNEL_CAPACITY,
};

pub struct SqliteConnection {
    path: PathBuf,
}

impl SqliteConnection {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

fn q_err(e: rusqlite::Error) -> QueryError {
    QueryError { code: None, message: e.to_string(), position: None }
}

fn value_to_text(v: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef::*;
    match v {
        Null => None,
        Integer(i) => Some(i.to_string()),
        Real(f) => Some(f.to_string()),
        Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        Blob(b) => Some(format!("<blob {} B>", b.len())),
    }
}

#[async_trait]
impl Connection for SqliteConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (schema_tx, schema_rx) = tokio::sync::oneshot::channel::<Result<SchemaRef, QueryError>>();
        let path = self.path.clone();
        let sql = sql.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = match rusqlite::Connection::open(&path) {
                Ok(c) => c,
                Err(e) => { let _ = schema_tx.send(Err(q_err(e))); return; }
            };
            // Watcher: protocol-level interrupt when the token fires.
            let interrupt = conn.get_interrupt_handle();
            let watcher_cancel = cancel.clone();
            let watcher = tokio::spawn(async move {
                watcher_cancel.cancelled().await;
                interrupt.interrupt();
            });

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => { let _ = schema_tx.send(Err(q_err(e))); watcher.abort(); return; }
            };
            let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let schema: SchemaRef = Arc::new(Schema::new(
                col_names.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect::<Vec<_>>(),
            ));
            let _ = schema_tx.send(Ok(schema.clone()));

            let ncols = col_names.len();
            let mut rows = match stmt.query([]) {
                Ok(r) => r,
                Err(e) => { let _ = tx.blocking_send(Err(q_err(e))); watcher.abort(); return; }
            };
            let mut builders: Vec<StringBuilder> =
                (0..ncols).map(|_| StringBuilder::new()).collect();
            let mut in_batch = 0usize;

            let flush = |builders: &mut Vec<StringBuilder>| -> RecordBatch {
                let arrays: Vec<ArrayRef> =
                    builders.iter_mut().map(|b| Arc::new(b.finish()) as ArrayRef).collect();
                RecordBatch::try_new(schema.clone(), arrays).expect("schema matches builders")
            };

            loop {
                match rows.next() {
                    Ok(Some(row)) => {
                        for (i, b) in builders.iter_mut().enumerate() {
                            match row.get_ref(i).ok().and_then(value_to_text) {
                                Some(s) => b.append_value(s),
                                None => b.append_null(),
                            }
                        }
                        in_batch += 1;
                        if in_batch >= BATCH_ROWS {
                            if tx.blocking_send(Ok(flush(&mut builders))).is_err() { break; }
                            in_batch = 0;
                        }
                    }
                    Ok(None) => {
                        if in_batch > 0 { let _ = tx.blocking_send(Ok(flush(&mut builders))); }
                        break;
                    }
                    Err(e) => {
                        let err = if cancel.is_cancelled() {
                            QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
                        } else { q_err(e) };
                        let _ = tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
            watcher.abort();
        });

        let columns = schema_rx.await.map_err(|_| QueryError::msg("driver task died"))??;
        Ok(QueryStream { columns, batches: rx })
    }

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&path).map_err(q_err)?;
            let mut tables = Vec::new();
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .map_err(q_err)?;
            let names: Vec<String> = stmt
                .query_map([], |r| r.get(0)).map_err(q_err)?
                .filter_map(Result::ok).collect();
            for name in names {
                let mut cstmt = conn
                    .prepare(&format!("PRAGMA table_info({})", name))
                    .map_err(q_err)?;
                let columns: Vec<ColumnInfo> = cstmt
                    .query_map([], |r| {
                        Ok(ColumnInfo { name: r.get(1)?, data_type: r.get(2)? })
                    })
                    .map_err(q_err)?
                    .filter_map(Result::ok)
                    .collect();
                tables.push(TableInfo { schema: None, name, columns });
            }
            Ok(SchemaSnapshot { tables })
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?
    }
}
```
Note the one subtlety: `tokio::spawn` for the watcher is called from inside `spawn_blocking` — that requires a runtime context. It works because `spawn_blocking` tasks inherit the runtime handle. If it panics with "no reactor running", capture `tokio::runtime::Handle::current()` before `spawn_blocking` and use `handle.spawn(...)`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p dbc-driver-sqlite`
Expected: PASS (4 tests). If the fixture fails on `generate_series`, apply the loop-insert fallback from Step 2 and re-run.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: sqlite driver with streaming batches and interrupt cancel"
```

---

### Task 6: Phase-1 UI — SQLite result in a virtualized grid

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml`, `crates/dbc-ui/src/main.rs`
- Create: `crates/dbc-ui/src/grid.rs`, `crates/dbc-ui/src/runner.rs`

**Interfaces:**
- Consumes: `SqliteConnection`, `dbc_core::*`, `ResultBuffer` (Task 4), gpui `uniform_list`.
- Produces:
  - `runner::QueryEvent` enum: `Started { columns: SchemaRef }`, `Batch(RecordBatch)`, `Finished { elapsed: Duration }`, `Failed(QueryError)`
  - `runner::QueryRunner::new() -> QueryRunner` (owns a tokio `Runtime`)
  - `QueryRunner::run(&self, conn: Box<dyn Connection>, sql: String, cancel: CancelToken) -> tokio::sync::mpsc::Receiver<QueryEvent>`
  - `grid::ResultGrid` view state consumed by `AppView` — reads from a shared `Rc<RefCell<ResultBuffer>>`.
- Phase-1 behavior: CLI arg 1 = path to a SQLite file. On launch the app immediately runs the hardcoded query `SELECT name, type FROM sqlite_master` (exists in every SQLite db) and shows it in the grid. Status bar shows `<n> rows`.

- [ ] **Step 1: Dependencies**

Add to `crates/dbc-ui/Cargo.toml`:
```toml
dbc-core = { path = "../dbc-core" }
dbc-buffer = { path = "../dbc-buffer" }
dbc-driver-sqlite = { path = "../dbc-driver-sqlite" }
tokio.workspace = true
```

- [ ] **Step 2: Implement the runner (the tokio↔GPUI bridge)**

`crates/dbc-ui/src/runner.rs`:
```rust
use std::time::{Duration, Instant};

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{CancelToken, Connection, QueryError};

pub enum QueryEvent {
    Started { columns: SchemaRef },
    Batch(RecordBatch),
    Finished { elapsed: Duration },
    Failed(QueryError),
}

/// Owns the tokio runtime. All DB I/O lives here; the UI thread only ever
/// awaits the event channel from inside `cx.spawn`.
pub struct QueryRunner {
    runtime: tokio::runtime::Runtime,
}

impl QueryRunner {
    pub fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime"),
        }
    }

    pub fn run(
        &self,
        mut conn: Box<dyn Connection>,
        sql: String,
        cancel: CancelToken,
    ) -> tokio::sync::mpsc::Receiver<QueryEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        self.runtime.spawn(async move {
            let started = Instant::now();
            match conn.query(&sql, cancel).await {
                Err(e) => { let _ = tx.send(QueryEvent::Failed(e)).await; }
                Ok(mut stream) => {
                    let _ = tx.send(QueryEvent::Started { columns: stream.columns.clone() }).await;
                    let mut failed = false;
                    while let Some(item) = stream.batches.recv().await {
                        match item {
                            Ok(b) => { let _ = tx.send(QueryEvent::Batch(b)).await; }
                            Err(e) => { let _ = tx.send(QueryEvent::Failed(e)).await; failed = true; }
                        }
                    }
                    if !failed {
                        let _ = tx.send(QueryEvent::Finished { elapsed: started.elapsed() }).await;
                    }
                }
            }
        });
        rx
    }
}
```

- [ ] **Step 3: Implement the grid**

`crates/dbc-ui/src/grid.rs`:
```rust
use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use gpui::{div, prelude::*, px, rgb, uniform_list, Context, Window};

pub const ROW_HEIGHT: f32 = 24.0;
pub const DEFAULT_COL_WIDTH: f32 = 160.0;

pub struct ResultGrid {
    pub buffer: Option<Rc<RefCell<ResultBuffer>>>,
    pub col_widths: Vec<f32>,
}

impl ResultGrid {
    pub fn new() -> Self {
        Self { buffer: None, col_widths: Vec::new() }
    }

    pub fn set_buffer(&mut self, buffer: Rc<RefCell<ResultBuffer>>) {
        let ncols = buffer.borrow().column_count();
        self.col_widths = vec![DEFAULT_COL_WIDTH; ncols];
        self.buffer = Some(buffer);
    }

    fn header(&self) -> impl IntoElement {
        let mut row = div().flex().flex_row().bg(rgb(0x313244)).text_color(rgb(0xf9e2af));
        if let Some(buf) = &self.buffer {
            let buf = buf.borrow();
            for (i, field) in buf.schema().fields().iter().enumerate() {
                row = row.child(
                    div()
                        .w(px(self.col_widths[i]))
                        .px_2()
                        .h(px(ROW_HEIGHT))
                        .overflow_hidden()
                        .child(field.name().clone()),
                );
            }
        }
        row
    }
}

impl Render for ResultGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.buffer.as_ref().map_or(0, |b| b.borrow().row_count());
        let buffer = self.buffer.clone();
        let widths = self.col_widths.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.header())
            .child(
                uniform_list(
                    "result-rows",
                    row_count,
                    cx.processor(move |_this, range: std::ops::Range<usize>, _window, _cx| {
                        let mut items = Vec::with_capacity(range.len());
                        if let Some(buf) = &buffer {
                            let mut buf = buf.borrow_mut();
                            let ncols = buf.column_count();
                            for row_ix in range {
                                let mut row = div()
                                    .id(row_ix)
                                    .flex()
                                    .flex_row()
                                    .h(px(ROW_HEIGHT))
                                    .bg(if row_ix % 2 == 0 { rgb(0x1e1e2e) } else { rgb(0x232334) });
                                for col in 0..ncols {
                                    row = row.child(
                                        div()
                                            .w(px(widths[col]))
                                            .px_2()
                                            .overflow_hidden()
                                            .text_color(rgb(0xcdd6f4))
                                            .child(buf.cell_text(row_ix, col)),
                                    );
                                }
                                items.push(row);
                            }
                        }
                        items
                    }),
                )
                .flex_1(),
            )
    }
}
```
API drift warning: `uniform_list` + `cx.processor` signatures follow the pinned rev's `crates/gpui/examples/uniform_list.rs`. If compilation disagrees, that example file in the cargo git checkout is the ground truth — adapt to it, don't fight it.

- [ ] **Step 4: Wire it in main**

Replace `crates/dbc-ui/src/main.rs`:
```rust
mod grid;
mod runner;

use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::CancelToken;
use dbc_driver_sqlite::SqliteConnection;
use gpui::{
    div, prelude::*, px, rgb, size, App, Bounds, Context, Entity, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use grid::ResultGrid;
use runner::{QueryEvent, QueryRunner};

struct AppView {
    grid: Entity<ResultGrid>,
    status: String,
}

impl AppView {
    fn run_startup_query(&mut self, db_path: String, cx: &mut Context<Self>) {
        let runner = QueryRunner::new();
        let conn = Box::new(SqliteConnection::new(db_path));
        let mut rx = runner.run(conn, "SELECT name, type FROM sqlite_master".into(), CancelToken::new());
        // Keep the runtime alive for the app's lifetime.
        std::mem::forget(runner); // phase 1 only; task 8 moves ownership into AppView
        let grid = self.grid.clone();
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |view, cx| {
                    match ev {
                        QueryEvent::Started { columns } => {
                            let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
                            buffer = Some(buf.clone());
                            grid.update(cx, |g, _| g.set_buffer(buf));
                            view.status = "running…".into();
                        }
                        QueryEvent::Batch(b) => {
                            if let Some(buf) = &buffer { buf.borrow_mut().push(b); }
                        }
                        QueryEvent::Finished { elapsed } => {
                            let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                            view.status = format!("{rows} rows in {elapsed:.2?}");
                        }
                        QueryEvent::Failed(e) => { view.status = format!("error: {e}"); }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .child(self.grid.clone())
            .child(
                div()
                    .h(px(28.))
                    .px_2()
                    .bg(rgb(0x313244))
                    .text_color(rgb(0xa6adc8))
                    .child(self.status.clone()),
            )
    }
}

fn main() {
    let db_path = std::env::args().nth(1).expect("usage: dbc-ui <sqlite-file>");
    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    let grid = cx.new(|_| ResultGrid::new());
                    let mut view = AppView { grid, status: "connecting…".into() };
                    view.run_startup_query(db_path, cx);
                    view
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
```
Note on `cx.spawn(async move |this, cx| ...)`: async-closure spawn signature follows the pinned rev. If it differs, check how `crates/gpui/examples` at the checkout call `cx.spawn` and match that.

- [ ] **Step 5: Manual verification (phase-1 gate)**

Create a scratch DB and run:
```powershell
cargo run -p dbc-ui -- path\to\any.db
```
Expected: window opens, grid shows `name`/`type` header plus a row per table, status bar shows `N rows in …`. Scroll works.
This step is the phase-1 verdict on GPUI ergonomics — note honest impressions in the commit message body.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: phase 1 — sqlite results in virtualized grid"
```

---

### Task 7: dbc-driver-postgres (Phase 2)

**Files:**
- Create: `crates/dbc-driver-postgres/Cargo.toml`, `src/lib.rs`, `src/types.rs`
- Create: `crates/dbc-driver-postgres/tests/integration.rs`
- Modify: root `Cargo.toml` members

**Interfaces:**
- Consumes: `dbc_core::*`.
- Produces: `PostgresConnection::connect(url: &str) -> Result<PostgresConnection, QueryError>` (async), implementing `dbc_core::Connection`.
- Design notes:
  - `prepare()` first → `Statement::columns()` gives names+types BEFORE any row (spec: header instantly). Then `query_raw` for a true `RowStream`.
  - Batch closes at `BATCH_ROWS` rows or `BATCH_LATENCY` after the batch's first row, whichever first (`tokio::select!` with `sleep_until`).
  - Cancel: `client.cancel_token()` before the query; watcher task calls `token.cancel_query(NoTls)` when fired. Server error 57014 (query_canceled) maps to `code: Some("cancelled")`.
  - Type mapping (in `src/types.rs`): BOOL→Boolean, INT2→Int16, INT4→Int32, INT8→Int64, FLOAT4→Float32, FLOAT8→Float64; TEXT/VARCHAR/BPCHAR/NAME→Utf8; NUMERIC via `rust_decimal`→Utf8; TIMESTAMP/TIMESTAMPTZ/DATE/TIME via `chrono`→Utf8 (ISO-8601); UUID via `uuid`→Utf8; anything else→Utf8 placeholder `"<oid N>"`. Column builders are an enum `ColBuilder { Bool(BooleanBuilder), I16(Int16Builder), I32(Int32Builder), I64(Int64Builder), F32(Float32Builder), F64(Float64Builder), Text(StringBuilder) }` with `append(row, idx)` and `finish() -> ArrayRef` methods.
  - `DbError` → `QueryError { code: sqlstate, message, position: from e.position() }`.

- [ ] **Step 1: Crate setup**

Add member. `Cargo.toml`:
```toml
[package]
name = "dbc-driver-postgres"
version = "0.1.0"
edition.workspace = true

[dependencies]
dbc-core = { path = "../dbc-core" }
tokio.workspace = true
tokio-util.workspace = true
async-trait.workspace = true
tokio-postgres = { version = "0.7", features = ["with-chrono-0_4", "with-uuid-1"] }
rust_decimal = { version = "1", features = ["db-tokio-postgres"] }
chrono = "0.4"
uuid = "1"
futures-util = "0.3"

[dev-dependencies]
testcontainers-modules = { version = "0.13", features = ["postgres"] }
tokio = { version = "1", features = ["full"] }
```

- [ ] **Step 2: Write failing integration tests**

`tests/integration.rs`:
```rust
//! Docker required. Run with: cargo test -p dbc-driver-postgres -- --ignored
use dbc_core::{CancelToken, Connection};
use dbc_driver_postgres::PostgresConnection;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

async fn pg_url(node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>) -> String {
    format!(
        "postgres://postgres:postgres@127.0.0.1:{}/postgres",
        node.get_host_port_ipv4(5432).await.unwrap()
    )
}

#[tokio::test]
#[ignore]
async fn streams_100k_rows_first_batch_early() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let started = std::time::Instant::now();
    let mut s = c
        .query("SELECT g AS id, 'row' || g AS name FROM generate_series(1, 100000) g", CancelToken::new())
        .await
        .unwrap();
    assert_eq!(s.columns.field(0).name(), "id");
    let first = s.batches.recv().await.unwrap().unwrap();
    let first_at = started.elapsed();
    let mut rows = first.num_rows();
    while let Some(b) = s.batches.recv().await { rows += b.unwrap().num_rows(); }
    assert_eq!(rows, 100_000);
    // First batch must arrive well before all 100k are done streaming.
    assert!(first_at.as_millis() < 1500, "first batch too late: {first_at:?}");
}

#[tokio::test]
#[ignore]
async fn typed_columns_come_back_typed() {
    use dbc_core::arrow::datatypes::DataType;
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let mut s = c
        .query("SELECT 1::int4 a, 2::int8 b, 3.5::float8 c, true d, 'x'::text e, 1.23::numeric f", CancelToken::new())
        .await
        .unwrap();
    let dts: Vec<DataType> = s.columns.fields().iter().map(|f| f.data_type().clone()).collect();
    assert_eq!(dts, vec![DataType::Int32, DataType::Int64, DataType::Float64, DataType::Boolean, DataType::Utf8, DataType::Utf8]);
    let b = s.batches.recv().await.unwrap().unwrap();
    assert_eq!(b.num_rows(), 1);
}

#[tokio::test]
#[ignore]
async fn cancel_kills_server_side_query() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let cancel = CancelToken::new();
    let mut s = c.query("SELECT pg_sleep(30)", cancel.clone()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    cancel.cancel();
    let mut cancelled = false;
    while let Some(r) = s.batches.recv().await {
        if let Err(e) = r { cancelled = e.code.as_deref() == Some("cancelled"); }
    }
    assert!(cancelled, "no cancelled error surfaced");
    assert!(t.elapsed().as_secs() < 5, "cancel took too long — not protocol-level");
}

#[tokio::test]
#[ignore]
async fn error_carries_sqlstate_and_position() {
    let node = Postgres::default().start().await.unwrap();
    let mut c = PostgresConnection::connect(&pg_url(&node).await).await.unwrap();
    let err = c.query("SELEC 1", CancelToken::new()).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("42601"));
    assert!(err.position.is_some());
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p dbc-driver-postgres -- --ignored`
Expected: compile error — `PostgresConnection` not defined. (If Docker is missing, compilation failure is still the expected signal here.)

- [ ] **Step 4: Implement `src/types.rs`**

```rust
use std::sync::Arc;
use dbc_core::arrow::array::{
    ArrayRef, BooleanBuilder, Float32Builder, Float64Builder, Int16Builder, Int32Builder,
    Int64Builder, StringBuilder,
};
use dbc_core::arrow::datatypes::DataType;
use tokio_postgres::types::Type;
use tokio_postgres::Row;

pub fn arrow_type(t: &Type) -> DataType {
    match *t {
        Type::BOOL => DataType::Boolean,
        Type::INT2 => DataType::Int16,
        Type::INT4 => DataType::Int32,
        Type::INT8 => DataType::Int64,
        Type::FLOAT4 => DataType::Float32,
        Type::FLOAT8 => DataType::Float64,
        _ => DataType::Utf8,
    }
}

pub enum ColBuilder {
    Bool(BooleanBuilder),
    I16(Int16Builder),
    I32(Int32Builder),
    I64(Int64Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    Text(StringBuilder),
}

impl ColBuilder {
    pub fn for_type(t: &Type) -> Self {
        match *t {
            Type::BOOL => Self::Bool(BooleanBuilder::new()),
            Type::INT2 => Self::I16(Int16Builder::new()),
            Type::INT4 => Self::I32(Int32Builder::new()),
            Type::INT8 => Self::I64(Int64Builder::new()),
            Type::FLOAT4 => Self::F32(Float32Builder::new()),
            Type::FLOAT8 => Self::F64(Float64Builder::new()),
            _ => Self::Text(StringBuilder::new()),
        }
    }

    pub fn append(&mut self, row: &Row, i: usize) {
        match self {
            Self::Bool(b) => b.append_option(row.get::<_, Option<bool>>(i)),
            Self::I16(b) => b.append_option(row.get::<_, Option<i16>>(i)),
            Self::I32(b) => b.append_option(row.get::<_, Option<i32>>(i)),
            Self::I64(b) => b.append_option(row.get::<_, Option<i64>>(i)),
            Self::F32(b) => b.append_option(row.get::<_, Option<f32>>(i)),
            Self::F64(b) => b.append_option(row.get::<_, Option<f64>>(i)),
            Self::Text(b) => b.append_option(text_value(row, i)),
        }
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::I16(b) => Arc::new(b.finish()),
            Self::I32(b) => Arc::new(b.finish()),
            Self::I64(b) => Arc::new(b.finish()),
            Self::F32(b) => Arc::new(b.finish()),
            Self::F64(b) => Arc::new(b.finish()),
            Self::Text(b) => Arc::new(b.finish()),
        }
    }
}

fn text_value(row: &Row, i: usize) -> Option<String> {
    let t = row.columns()[i].type_();
    match *t {
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            row.get::<_, Option<String>>(i)
        }
        Type::NUMERIC => row
            .get::<_, Option<rust_decimal::Decimal>>(i)
            .map(|d| d.to_string()),
        Type::TIMESTAMP => row
            .get::<_, Option<chrono::NaiveDateTime>>(i)
            .map(|v| v.to_string()),
        Type::TIMESTAMPTZ => row
            .get::<_, Option<chrono::DateTime<chrono::Utc>>>(i)
            .map(|v| v.to_rfc3339()),
        Type::DATE => row.get::<_, Option<chrono::NaiveDate>>(i).map(|v| v.to_string()),
        Type::TIME => row.get::<_, Option<chrono::NaiveTime>>(i).map(|v| v.to_string()),
        Type::UUID => row.get::<_, Option<uuid::Uuid>>(i).map(|v| v.to_string()),
        _ => Some(format!("<oid {}>", t.oid())),
    }
}
```

- [ ] **Step 5: Implement `src/lib.rs`**

```rust
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::{Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, QueryError, QueryStream, SchemaSnapshot, TableInfo,
    BATCH_LATENCY, BATCH_ROWS, CHANNEL_CAPACITY,
};
use futures_util::StreamExt;
use tokio_postgres::NoTls;
use types::{arrow_type, ColBuilder};

pub struct PostgresConnection {
    client: tokio_postgres::Client,
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
        Ok(Self { client })
    }
}

#[async_trait]
impl Connection for PostgresConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        // prepare() gives us column names AND types before the first row.
        let stmt = self.client.prepare(sql).await.map_err(pg_err)?;
        let fields: Vec<Field> = stmt
            .columns()
            .iter()
            .map(|c| Field::new(c.name(), arrow_type(c.type_()), true))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let col_types: Vec<tokio_postgres::types::Type> =
            stmt.columns().iter().map(|c| c.type_().clone()).collect();

        // Protocol-level cancel goes over a separate connection.
        let cancel_handle = self.client.cancel_token();
        let watcher_cancel = cancel.clone();
        tokio::spawn(async move {
            watcher_cancel.cancelled().await;
            let _ = cancel_handle.cancel_query(NoTls).await;
        });

        let params: Vec<String> = Vec::new();
        let mut row_stream = self
            .client
            .query_raw(&stmt, params)
            .await
            .map_err(pg_err)?;

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let batch_schema = schema.clone();
        tokio::spawn(async move {
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

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let rows = self
            .client
            .query(
                "SELECT table_schema, table_name, column_name, data_type
                 FROM information_schema.columns
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
                 ORDER BY table_schema, table_name, ordinal_position",
                &[],
            )
            .await
            .map_err(pg_err)?;
        let mut tables: Vec<TableInfo> = Vec::new();
        for row in rows {
            let (ts, tn): (String, String) = (row.get(0), row.get(1));
            let col = ColumnInfo { name: row.get(2), data_type: row.get(3) };
            match tables.last_mut() {
                Some(t) if t.schema.as_deref() == Some(&ts) && t.name == tn => t.columns.push(col),
                _ => tables.push(TableInfo { schema: Some(ts), name: tn, columns: vec![col] }),
            }
        }
        Ok(SchemaSnapshot { tables })
    }
}
```

- [ ] **Step 6: Run integration tests (Docker running)**

Run: `cargo test -p dbc-driver-postgres -- --ignored`
Expected: 4 PASS. Also run plain `cargo test` — expected: everything still green, pg tests skipped.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: postgres driver — typed streaming, latency flush, protocol cancel"
```

---

### Task 8: SQL input + keybindings + connection dispatch (Phase 2)

**Files:**
- Create: `crates/dbc-ui/src/sql_input.rs`
- Create: `crates/dbc-ui/src/connect.rs`
- Modify: `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/Cargo.toml`

**Interfaces:**
- Consumes: `PostgresConnection::connect(url)`, `SqliteConnection::new(path)`, `QueryRunner` (Task 6), `runner::QueryEvent`.
- Produces:
  - `connect::open(url: &str, runtime: &tokio::runtime::Handle) -> Result<Box<dyn dbc_core::Connection>, dbc_core::QueryError>` — dispatch rule: starts with `postgres://` or `postgresql://` → Postgres (connect awaited on the runtime); anything else → treated as a SQLite file path.
  - `sql_input::SqlInput` view: single-line editable text field with `focus_handle`, `content() -> String`, emits nothing itself — `AppView` reads content when the `RunQuery` action fires.
  - gpui actions `RunQuery` (ctrl-enter) and `CancelQuery` (escape), bound app-wide.
- Behavior change: CLI arg 1 is now ANY connection string (pg URL or sqlite path). The startup auto-query is removed; user types SQL and hits Ctrl+Enter. Esc cancels the running query (`CancelToken` stored on `AppView`, fresh per run).
- SQL input implementation: copy `crates/gpui/examples/input.rs` from the pinned Zed checkout (path printed by `cargo metadata` or under `%USERPROFILE%\.cargo\git\checkouts\zed-*\<rev>\crates\gpui\examples\input.rs`) into `sql_input.rs`, then: delete the wasm cfg blocks and `main()`, rename `TextInput` → `SqlInput`, keep all editing actions (backspace/delete/arrows/select/copy/paste), and re-scope the `actions!` macro namespace to `sql_input`. This file is large (~500 lines) — that is expected; a from-scratch text field is phase-5 folly, Zed's example is the sanctioned crib.
- Single-line is accepted for v1 (multiline editor is phase 5); pasted newlines are replaced with spaces in `content()`.

- [ ] **Step 1: Add dependency**

Add to `crates/dbc-ui/Cargo.toml`:
```toml
dbc-driver-postgres = { path = "../dbc-driver-postgres" }
unicode-segmentation = "1"
```

- [ ] **Step 2: Implement `connect.rs`**

```rust
use dbc_core::{Connection, QueryError};
use dbc_driver_postgres::PostgresConnection;
use dbc_driver_sqlite::SqliteConnection;

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
```
Note: `block_on` here is called from the UI thread ONCE at startup, before the window opens — acceptable for phase 2 (connection manager with async connect UI is phase 4). Expose `QueryRunner::handle(&self) -> tokio::runtime::Handle` (add `pub fn handle(&self) -> tokio::runtime::Handle { self.runtime.handle().clone() }` to `runner.rs`) and stop `mem::forget`ing the runner — `AppView` now owns `runner: QueryRunner`.

- [ ] **Step 3: Port `input.rs` → `sql_input.rs`** (per the interface notes above)

Add a public accessor at the bottom of the ported view:
```rust
impl SqlInput {
    pub fn text(&self) -> String {
        self.content.replace('\n', " ")
    }
}
```

- [ ] **Step 4: Actions and wiring in `main.rs`**

Key additions (integrate into the existing `AppView` from Task 6):
```rust
use gpui::{actions, KeyBinding};

actions!(dbc, [RunQuery, CancelQuery]);

// in application().run, before open_window:
cx.bind_keys([
    KeyBinding::new("ctrl-enter", RunQuery, None),
    KeyBinding::new("escape", CancelQuery, None),
]);
```
`AppView` gains fields: `runner: QueryRunner`, `conn_url: String`, `sql: Entity<SqlInput>`, `cancel: Option<CancelToken>`, `started_at: Option<std::time::Instant>`.
`AppView::render` root div gets `.on_action(cx.listener(Self::on_run_query)).on_action(cx.listener(Self::on_cancel_query))` and a top bar `div().h(px(36.)).child(self.sql.clone())` above the grid.

```rust
impl AppView {
    fn on_run_query(&mut self, _: &RunQuery, _window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel.is_some() { return; } // one query at a time in v1
        let sql = self.sql.read(cx).text();
        if sql.trim().is_empty() { return; }
        let conn = match connect::open(&self.conn_url, &self.runner.handle()) {
            Ok(c) => c,
            Err(e) => { self.status = format!("error: {e}"); cx.notify(); return; }
        };
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.started_at = Some(std::time::Instant::now());
        let mut rx = self.runner.run(conn, sql, cancel);
        let grid = self.grid.clone();
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |view, cx| {
                    match ev {
                        QueryEvent::Started { columns } => {
                            let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
                            buffer = Some(buf.clone());
                            grid.update(cx, |g, _| g.set_buffer(buf));
                        }
                        QueryEvent::Batch(b) => {
                            if let Some(buf) = &buffer { buf.borrow_mut().push(b); }
                            let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                            let secs = view.started_at.map_or(0.0, |t| t.elapsed().as_secs_f32());
                            view.status = format!("{rows} rows… {secs:.1}s");
                        }
                        QueryEvent::Finished { elapsed } => {
                            let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                            view.status = format!("{rows} rows in {elapsed:.2?}");
                            view.cancel = None;
                        }
                        QueryEvent::Failed(e) => {
                            view.status = format!("error: {e}");
                            view.cancel = None;
                        }
                    }
                    cx.notify();
                });
            }
            let _ = this.update(cx, |view, cx| { view.cancel = None; cx.notify(); });
        })
        .detach();
    }

    fn on_cancel_query(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
            self.status = "cancelling…".into();
            cx.notify();
        }
    }
}
```
Remove `run_startup_query` and the `mem::forget`.

- [ ] **Step 5: Manual verification**

```powershell
# sqlite
cargo run -p dbc-ui -- path\to\any.db
# postgres (docker run --rm -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:17)
cargo run -p dbc-ui -- postgres://postgres:postgres@localhost:5432/postgres
```
Checklist: type `select generate_series(1, 100000)` → Ctrl+Enter → rows appear immediately and count climbs; `select pg_sleep(30)` → Esc within a second shows `error: [cancelled] …`; a syntax error shows `[42601] … (at N)` in the status bar; typing/editing/copy/paste in the input works.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: SQL input, ctrl-enter run, esc cancel, pg/sqlite dispatch"
```

---

### Task 9: Buffer spill to disk + 1M-row proof (Phase 2)

**Files:**
- Modify: `crates/dbc-buffer/src/lib.rs`, `crates/dbc-buffer/Cargo.toml`
- Create: `crates/dbc-buffer/benches/push_1m.rs`

**Interfaces:**
- Consumes: existing `ResultBuffer` API (unchanged for callers — spill is internal).
- Produces: `ResultBuffer::with_cap(schema: SchemaRef, cap_rows: usize) -> ResultBuffer`; `new()` keeps default cap `500_000`. Existing `push` / `cell_text` / `row_count` signatures unchanged.
- Design: once in-memory rows exceed `cap_rows`, further batches are written each to its own Arrow IPC file (`spill-<i>.arrow`) in a `tempfile::TempDir` owned by the buffer (auto-deleted on drop). `locate()` gains a batch source: `enum Slot { Mem(RecordBatch), Spilled { file_ix: usize, rows: usize } }`. Reads go through a one-slot cache `(usize, RecordBatch)` — the grid reads sequential windows, so one cached batch eliminates nearly all re-reads.

- [ ] **Step 1: Write failing test**

Add to tests in `crates/dbc-buffer/src/lib.rs`:
```rust
#[test]
fn spills_past_cap_and_reads_back() {
    let b0 = batch(0, 100);
    let mut buf = ResultBuffer::with_cap(b0.schema(), 150); // cap at 150 rows
    buf.push(b0);                 // 100 mem
    buf.push(batch(100, 100));    // 200 total → this batch spills
    buf.push(batch(200, 100));    // spills
    assert_eq!(buf.row_count(), 300);
    assert_eq!(buf.cell_text(50, 0), "50");    // mem
    assert_eq!(buf.cell_text(150, 0), "150");  // spilled
    assert_eq!(buf.cell_text(299, 0), "299");  // spilled, different file
    assert_eq!(buf.cell_text(150, 1), "row150");
    // cache flip-flop: read across files repeatedly
    assert_eq!(buf.cell_text(299, 0), "299");
    assert_eq!(buf.cell_text(150, 0), "150");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p dbc-buffer`
Expected: compile error — `with_cap` not defined.

- [ ] **Step 3: Implement**

Add deps to `crates/dbc-buffer/Cargo.toml`: `tempfile = "3"`. Rework internals:
```rust
use std::fs::File;
use std::io::BufReader;
use dbc_core::arrow::ipc::reader::FileReader;
use dbc_core::arrow::ipc::writer::FileWriter;

enum Slot {
    Mem(RecordBatch),
    Spilled { file_ix: usize },
}

pub struct ResultBuffer {
    schema: SchemaRef,
    slots: Vec<Slot>,
    offsets: Vec<usize>,
    cap_rows: usize,
    mem_rows: usize,
    spill_dir: Option<tempfile::TempDir>,
    spill_files: usize,
    cache: Option<(usize, RecordBatch)>, // (slot index, batch)
}
```
`new(schema)` → `Self::with_cap(schema, 500_000)`. `push`:
```rust
pub fn push(&mut self, batch: RecordBatch) {
    let n = batch.num_rows();
    let total = self.offsets.last().copied().unwrap_or(0) + n;
    self.offsets.push(total);
    if self.mem_rows + n <= self.cap_rows {
        self.mem_rows += n;
        self.slots.push(Slot::Mem(batch));
    } else {
        let dir = self.spill_dir.get_or_insert_with(|| tempfile::tempdir().expect("spill dir"));
        let path = dir.path().join(format!("spill-{}.arrow", self.spill_files));
        let file = File::create(&path).expect("spill file");
        let mut w = FileWriter::try_new(file, &self.schema).expect("ipc writer");
        w.write(&batch).expect("spill write");
        w.finish().expect("spill finish");
        self.slots.push(Slot::Spilled { file_ix: self.spill_files });
        self.spill_files += 1;
    }
}
```
`cell_text` resolves via `locate`, then:
```rust
fn slot_batch(&mut self, slot_ix: usize) -> &RecordBatch {
    if let Slot::Mem(b) = &self.slots[slot_ix] {
        return b; // borrow checker: return early for mem slots
    }
    if self.cache.as_ref().map(|(i, _)| *i) != Some(slot_ix) {
        let Slot::Spilled { file_ix } = self.slots[slot_ix] else { unreachable!() };
        let path = self
            .spill_dir.as_ref().expect("spill dir exists")
            .path().join(format!("spill-{file_ix}.arrow"));
        let reader = FileReader::try_new(BufReader::new(File::open(path).expect("spill open")), None)
            .expect("ipc reader");
        let batch = reader.into_iter().next().expect("one batch").expect("read batch");
        self.cache = Some((slot_ix, batch));
    }
    &self.cache.as_ref().unwrap().1
}
```
(If borrowck fights the early-return pattern, restructure to compute a `bool` first — mechanics over elegance here.) The `expect`s are fine: spill I/O failure on a local temp dir is a programmer/environment error, not a query error.

- [ ] **Step 4: Run tests**

Run: `cargo test -p dbc-buffer`
Expected: PASS (3 tests).

- [ ] **Step 5: Criterion bench guarding the Arrow decision**

Add to `Cargo.toml`:
```toml
[dev-dependencies]
criterion = "0.5"
tempfile = "3"

[[bench]]
name = "push_1m"
harness = false
```
`benches/push_1m.rs`:
```rust
use criterion::{criterion_group, criterion_main, Criterion};
use dbc_buffer::ResultBuffer;
use dbc_core::arrow::array::{Int64Array, RecordBatch, StringArray};
use dbc_core::arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

fn bench_1m(c: &mut Criterion) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
    ]));
    c.bench_function("push_1m_rows_then_1k_reads", |b| {
        b.iter(|| {
            let mut buf = ResultBuffer::new(schema.clone());
            for chunk in 0..1000 {
                let start = chunk as i64 * 1000;
                let ids = Int64Array::from_iter_values(start..start + 1000);
                let names = StringArray::from_iter_values((0..1000).map(|i| format!("row{}", start + i)));
                buf.push(RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(names)]).unwrap());
            }
            assert_eq!(buf.row_count(), 1_000_000);
            for i in (0..1_000_000).step_by(1000) {
                std::hint::black_box(buf.cell_text(i, 0));
            }
        })
    });
}

criterion_group!(benches, bench_1m);
criterion_main!(benches);
```
Run: `cargo bench -p dbc-buffer`
Expected: completes; record the time in the commit message. (This bench crosses the 500k default cap, so it also exercises spill.)

- [ ] **Step 6: Manual 1M-row end-to-end**

```powershell
cargo run --release -p dbc-ui -- postgres://postgres:postgres@localhost:5432/postgres
```
Query: `SELECT g AS id, md5(g::text) AS hash FROM generate_series(1, 1000000) g`
Expected: first rows visible immediately; count climbs to 1,000,000; scrolling anywhere in the grid stays smooth (spilled regions included); app memory stays far below raw-row cost (Task Manager sanity check).

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: buffer spill to disk + 1M-row bench"
```

---

### Task 10: Grid interactions — column resize + cell copy (Phase 2 close-out)

**Files:**
- Modify: `crates/dbc-ui/src/grid.rs`

**Interfaces:**
- Consumes: existing `ResultGrid`, gpui mouse events, `cx.write_to_clipboard` / `ClipboardItem`.
- Produces (spec §5 completion): draggable column resize; click-to-select cell; shift-click rectangular range; `ctrl-c` copies selection as TSV (rows separated by `\n`, cells by `\t`).
- State added to `ResultGrid`: `selection: Option<((usize, usize), (usize, usize))>` (anchor cell, focus cell — normalize min/max when copying), `resizing: Option<(usize, f32, f32)>` (col index, start mouse x, start width).

- [ ] **Step 1: Column resize**

In `header()`, after each header cell, render a 5px handle:
```rust
div()
    .id(("resize", i))
    .w(px(5.))
    .h(px(ROW_HEIGHT))
    .cursor_col_resize()
    .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, e: &gpui::MouseDownEvent, _w, _cx| {
        this.resizing = Some((i, e.position.x.into(), this.col_widths[i]));
    }))
```
On the grid's root div, while `self.resizing.is_some()`, attach:
```rust
.on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _w, cx| {
    if let Some((col, start_x, start_w)) = this.resizing {
        let dx: f32 = f32::from(e.position.x) - start_x;
        this.col_widths[col] = (start_w + dx).max(40.0);
        cx.notify();
    }
}))
.on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _e, _w, cx| {
    this.resizing = None;
    cx.notify();
}))
```
(`header()` needs `cx: &mut Context<Self>` passed in for `cx.listener` — change its signature to `fn header(&self, cx: &mut Context<Self>) -> impl IntoElement` and call it from `render` accordingly. Exact `Pixels`→`f32` conversions per the pinned rev; `f32::from(px)` or `.0` — check `crates/gpui/src/geometry.rs` if it disagrees.)

- [ ] **Step 2: Cell selection + copy**

Each data cell gets:
```rust
.id(("cell", row_ix * 10_000 + col)) // unique element id
.on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
    if e.modifiers.shift {
        if let Some((anchor, _)) = this.selection { this.selection = Some((anchor, (row_ix, col))); }
        else { this.selection = Some(((row_ix, col), (row_ix, col))); }
    } else {
        this.selection = Some(((row_ix, col), (row_ix, col)));
    }
    cx.notify();
}))
```
Selected cells get `.bg(rgb(0x45475a))` (compute `is_selected(row_ix, col)` from the normalized rectangle). Note the closure capture: `cx.processor` gives `_this` — use it instead of a captured clone where the borrow checker requires; selection state lives on `ResultGrid` so the processor closure signature from Task 6 changes from `|_this, range, ...|` to `|this, range, ...|` with cell listeners built via a helper on `this`.

Add a `CopySelection` action bound to `ctrl-c` in the same `actions!`/`bind_keys` block as Task 8 (namespace `dbc`), handled on `ResultGrid` (root div `.on_action(cx.listener(Self::on_copy))`):
```rust
fn on_copy(&mut self, _: &CopySelection, _w: &mut Window, cx: &mut Context<Self>) {
    let Some(((r0, c0), (r1, c1))) = self.selection else { return };
    let (rmin, rmax) = (r0.min(r1), r0.max(r1));
    let (cmin, cmax) = (c0.min(c1), c0.max(c1));
    let Some(buf) = &self.buffer else { return };
    let mut buf = buf.borrow_mut();
    let mut out = String::new();
    for r in rmin..=rmax {
        for c in cmin..=cmax {
            if c > cmin { out.push('\t'); }
            out.push_str(&buf.cell_text(r, c));
        }
        out.push('\n');
    }
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(out));
}
```

- [ ] **Step 3: Manual verification**

Run against postgres with a 10k-row query. Checklist: drag a header handle → column widens/narrows, min 40px; click cell → highlighted; shift-click another → rectangle highlighted; ctrl-c → paste into notepad gives TSV; resize during an actively streaming query doesn't hitch scrolling.

- [ ] **Step 4: Commit — phases 0–2 complete**

```bash
git add -A && git commit -m "feat: column resize + cell selection/copy — phases 0-2 complete"
```

---

## Self-Review Notes

- Spec coverage: §2 phases 0/1/2 → Tasks 1–2 / 3–6 / 7–10. §3 crate layout + trait → Tasks 1, 3 (layering enforced in Global Constraints). §4 threading → Tasks 6, 8; first-row-fast → Task 7 (prepare + latency flush, asserted in integration test); cancellation → Tasks 5, 7, 8 (tested at driver level, wired to Esc); buffer + spill → Tasks 4, 9; errors-as-data with position → Tasks 3, 7 (asserted). §5 UI → Tasks 6, 8, 10 (input, grid, status bar, resize, copy). §7 testing → fake-free unit tests (buffer/sqlite need no fake driver — sqlite in-memory files serve), testcontainers `#[ignore]`, criterion bench. Spec's "unit tests against a fake driver" for dbc-core: dbc-core is pure types; the drivers' own tests exercise the trait — recorded here as a conscious deviation, not an oversight.
- Known API-drift risk is concentrated in GPUI call sites (Tasks 2, 6, 8, 10); each carries a pointer to the ground-truth example file in the pinned checkout.
- Type consistency pass done: `QueryEvent` variants, `ResultBuffer` methods (`cell_text(&mut self, …)` everywhere), `CancelToken` clone-then-cancel pattern, `connect::open` signature match across Tasks 6/8/9/10.
