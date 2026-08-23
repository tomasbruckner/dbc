//! JSON shaping + size caps for the two tools that return payloads:
//! `get_schema` (§5/§6 512 KB cap) and `run_query` (§6 2 MB cap, array-of-
//! arrays row shape).

use dbc_buffer::ResultBuffer;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{RoutineInfo, SchemaSnapshot, SequenceInfo, TableInfo, TriggerInfo};
use serde_json::{json, Value};

/// Design doc §5: "if the result exceeds 512 KB, truncate the `tables`
/// array".
pub const SCHEMA_BYTE_CAP: usize = 512 * 1024;
/// Design doc §5/§6: "Response byte cap: 2 MB serialized, independent of
/// the row cap".
pub const ROW_BYTE_CAP: usize = 2 * 1024 * 1024;

/// `schema: Option<String>` filter semantics (§5): items whose own `schema`
/// field is `None` (e.g. every SQLite table) are always kept — the filter
/// has nothing to compare against, so it is a no-op for them, exactly as
/// documented. Items with `schema: Some(_)` are kept only on an exact
/// match.
fn keep(item_schema: &Option<String>, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(f) => item_schema.as_deref().map(|s| s == f).unwrap_or(true),
    }
}

/// Shapes a `SchemaSnapshot` into `get_schema`'s documented JSON contract:
/// applies the `schema` filter, strips every `ddl` field unless
/// `include_ddl`, and truncates to the 512 KB cap if the full payload would
/// exceed it (§5).
pub fn schema_to_json(snapshot: &SchemaSnapshot, schema_filter: Option<&str>, include_ddl: bool) -> Value {
    let mut tables: Vec<TableInfo> =
        snapshot.tables.iter().filter(|t| keep(&t.schema, schema_filter)).cloned().collect();
    let routines: Vec<RoutineInfo> =
        snapshot.routines.iter().filter(|r| keep(&r.schema, schema_filter)).cloned().collect();
    let triggers: Vec<TriggerInfo> =
        snapshot.triggers.iter().filter(|t| keep(&t.schema, schema_filter)).cloned().collect();
    let sequences: Vec<SequenceInfo> =
        snapshot.sequences.iter().filter(|s| keep(&s.schema, schema_filter)).cloned().collect();

    let mut routines = routines;
    let mut triggers = triggers;
    if !include_ddl {
        for t in &mut tables {
            t.ddl = None;
        }
        for r in &mut routines {
            r.ddl = None;
        }
        for t in &mut triggers {
            t.ddl = None;
        }
    }

    // "keep the first N by (schema, name) sort order" (§5).
    tables.sort_by(|a, b| {
        (a.schema.as_deref().unwrap_or(""), a.name.as_str())
            .cmp(&(b.schema.as_deref().unwrap_or(""), b.name.as_str()))
    });

    let full = json!({
        "tables": tables,
        "routines": routines,
        "triggers": triggers,
        "sequences": sequences,
        "truncated": false,
    });
    if byte_len(&full) <= SCHEMA_BYTE_CAP {
        return full;
    }

    truncate_tables(tables)
}

fn byte_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX)
}

/// §5's single-pass truncation: `routines`/`triggers`/`sequences` are
/// dropped entirely (not truncated piecemeal), and `tables` keeps as many
/// (already-sorted) entries as fit under the cap.
fn truncate_tables(tables: Vec<TableInfo>) -> Value {
    let total = tables.len();
    let overhead = byte_len(&json!({
        "tables": Vec::<TableInfo>::new(),
        "truncated": true,
        "tables_returned": 0,
        "tables_total": total,
    }));
    let mut running = overhead;
    let mut kept = Vec::new();
    for t in tables {
        let t_bytes = byte_len(&json!(t));
        if running + t_bytes > SCHEMA_BYTE_CAP && !kept.is_empty() {
            break;
        }
        running += t_bytes;
        kept.push(t);
    }
    let n = kept.len();
    json!({
        "tables": kept,
        "truncated": true,
        "tables_returned": n,
        "tables_total": total,
    })
}

/// A drained-and-capped query result, ready to serialize. `row_limit_hit`
/// is set by the caller when the drain loop stopped early because
/// `row_limit` rows were reached (§5's "real hard cap is at result
/// consumption").
pub struct DrainedResult {
    pub columns: SchemaRef,
    pub buffer: ResultBuffer,
    pub row_limit_hit: bool,
    pub duration_ms: u64,
}

/// Shapes a drained `ResultBuffer` into `run_query`'s documented
/// array-of-arrays JSON contract (§6), applying the independent 2 MB byte
/// cap (§5/§6) on top of whatever `row_limit_hit` already did. Every cell
/// is a JSON string or `null`, never a number/bool, via
/// `cell_text`/`cell_is_null` — the existing, already-tested text-cell
/// pipeline (`dbc-buffer`).
pub fn rows_to_json(result: DrainedResult) -> Value {
    let DrainedResult { columns, mut buffer, row_limit_hit, duration_ms } = result;

    let columns_json: Vec<Value> = columns
        .fields()
        .iter()
        .map(|f| json!({"name": f.name(), "type": f.data_type().to_string()}))
        .collect();

    let base_overhead = byte_len(&json!({
        "columns": columns_json,
        "rows": Vec::<Value>::new(),
        "row_count": 0,
        "truncated": row_limit_hit,
        "duration_ms": duration_ms,
    }));

    let total_rows = buffer.row_count();
    let ncols = buffer.column_count();
    let mut running = base_overhead;
    let mut rows_json: Vec<Value> = Vec::new();
    let mut byte_cap_hit = false;
    for r in 0..total_rows {
        let mut row = Vec::with_capacity(ncols);
        for c in 0..ncols {
            if buffer.cell_is_null(r, c) {
                row.push(Value::Null);
            } else {
                row.push(Value::String(buffer.cell_text(r, c)));
            }
        }
        let row_bytes = byte_len(&json!(row));
        if running + row_bytes > ROW_BYTE_CAP && !rows_json.is_empty() {
            byte_cap_hit = true;
            break;
        }
        running += row_bytes;
        rows_json.push(json!(row));
    }

    let row_count = rows_json.len();
    json!({
        "columns": columns_json,
        "rows": rows_json,
        "row_count": row_count,
        "truncated": row_limit_hit || byte_cap_hit,
        "duration_ms": duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{ColumnInfo, TableKind};
    use std::sync::Arc;

    fn table(schema: Option<&str>, name: &str, ddl: Option<&str>) -> TableInfo {
        TableInfo {
            schema: schema.map(|s| s.to_string()),
            name: name.to_string(),
            kind: TableKind::Table,
            columns: vec![ColumnInfo {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
                default: None,
                is_pk: true,
                fk: None,
            }],
            indexes: vec![],
            constraints: vec![],
            ddl: ddl.map(|s| s.to_string()),
        }
    }

    #[test]
    fn schema_filter_matches_by_schema_and_is_noop_for_none() {
        let snap = SchemaSnapshot {
            tables: vec![
                table(Some("public"), "a", None),
                table(Some("other"), "b", None),
                table(None, "sqlite_table", None), // e.g. SQLite
            ],
            ..Default::default()
        };
        let v = schema_to_json(&snap, Some("public"), false);
        let names: Vec<&str> = v["tables"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"a"));
        assert!(!names.contains(&"b"));
        // Schema-less table is unaffected by the filter (documented no-op).
        assert!(names.contains(&"sqlite_table"));
    }

    #[test]
    fn include_ddl_defaults_to_stripped() {
        let snap = SchemaSnapshot { tables: vec![table(None, "t", Some("CREATE TABLE t(...)"))], ..Default::default() };
        let without = schema_to_json(&snap, None, false);
        assert!(without["tables"][0]["ddl"].is_null());
        let with = schema_to_json(&snap, None, true);
        assert_eq!(with["tables"][0]["ddl"], "CREATE TABLE t(...)");
    }

    #[test]
    fn oversized_snapshot_trips_truncation_marker() {
        // Each table's ddl is padded well past what fits in 512 KB total
        // across a few hundred tables — forces the truncation path without
        // needing a real huge DB (T5).
        let big_ddl = "x".repeat(5_000);
        let tables: Vec<TableInfo> =
            (0..200).map(|i| table(None, &format!("t{i}"), Some(&big_ddl))).collect();
        let snap = SchemaSnapshot { tables, ..Default::default() };
        let v = schema_to_json(&snap, None, true); // include_ddl so the padding survives
        assert_eq!(v["truncated"], true);
        assert_eq!(v["tables_total"], 200);
        let returned = v["tables_returned"].as_u64().unwrap();
        assert!(returned > 0 && returned < 200, "expected partial truncation, got {returned}");
        assert_eq!(v["tables"].as_array().unwrap().len(), returned as usize);
        // routines/triggers/sequences dropped entirely once truncation hits.
        assert!(v.get("routines").is_none());

        let bytes = serde_json::to_vec(&v).unwrap().len();
        assert!(bytes <= SCHEMA_BYTE_CAP + 10_000, "truncated payload should be close to the cap, got {bytes}");
    }

    #[test]
    fn small_snapshot_not_truncated() {
        let snap = SchemaSnapshot { tables: vec![table(None, "t", None)], ..Default::default() };
        let v = schema_to_json(&snap, None, false);
        assert_eq!(v["truncated"], false);
        assert!(v.get("tables_returned").is_none());
    }

    fn schema_ref() -> SchemaRef {
        use dbc_core::arrow::datatypes::{DataType, Field, Schema};
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
        ]))
    }

    fn batch(rows: &[(Option<&str>, Option<&str>)]) -> dbc_core::arrow::array::RecordBatch {
        use dbc_core::arrow::array::{RecordBatch, StringArray};
        let ids = StringArray::from_iter(rows.iter().map(|(a, _)| *a));
        let names = StringArray::from_iter(rows.iter().map(|(_, b)| *b));
        RecordBatch::try_new(schema_ref(), vec![Arc::new(ids), Arc::new(names)]).unwrap()
    }

    #[test]
    fn rows_serialize_as_array_of_arrays_with_null_distinct_from_empty_string() {
        let mut buf = ResultBuffer::with_cap(schema_ref(), 10);
        buf.push(batch(&[(Some("1"), Some("Alice")), (Some("2"), None), (Some("3"), Some(""))])).unwrap();
        let v = rows_to_json(DrainedResult { columns: schema_ref(), buffer: buf, row_limit_hit: false, duration_ms: 4 });
        assert_eq!(v["row_count"], 3);
        assert_eq!(v["rows"][0], json!(["1", "Alice"]));
        assert_eq!(v["rows"][1], json!(["2", null]));
        assert_eq!(v["rows"][2], json!(["3", ""])); // empty string != null
        assert_eq!(v["truncated"], false);
        assert_eq!(v["columns"][0]["name"], "id");
    }

    #[test]
    fn row_limit_hit_flag_marks_truncated() {
        let mut buf = ResultBuffer::with_cap(schema_ref(), 10);
        buf.push(batch(&[(Some("1"), Some("a"))])).unwrap();
        let v = rows_to_json(DrainedResult { columns: schema_ref(), buffer: buf, row_limit_hit: true, duration_ms: 1 });
        assert_eq!(v["truncated"], true);
        assert_eq!(v["row_count"], 1);
    }

    #[test]
    fn byte_cap_truncates_rows_independent_of_row_limit() {
        let mut buf = ResultBuffer::with_cap(schema_ref(), 10_000);
        let big = "y".repeat(200_000);
        let rows: Vec<(Option<&str>, Option<&str>)> = vec![(Some("1"), Some(&big)); 20];
        buf.push(batch(&rows)).unwrap();
        let v = rows_to_json(DrainedResult { columns: schema_ref(), buffer: buf, row_limit_hit: false, duration_ms: 1 });
        assert_eq!(v["truncated"], true);
        let returned = v["row_count"].as_u64().unwrap();
        assert!(returned > 0 && returned < 20, "expected partial rows under the byte cap, got {returned}");
        let bytes = serde_json::to_vec(&v).unwrap().len();
        assert!(bytes <= ROW_BYTE_CAP + 300_000, "should stop near the cap, got {bytes}");
    }
}
