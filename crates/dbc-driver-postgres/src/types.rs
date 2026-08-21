use std::error::Error as StdError;
use std::sync::Arc;
use dbc_core::arrow::array::{
    ArrayRef, BooleanBuilder, Float32Builder, Float64Builder, Int16Builder, Int32Builder,
    Int64Builder, StringBuilder,
};
use dbc_core::arrow::datatypes::DataType;
use tokio_postgres::types::{FromSql, Type};
use tokio_postgres::Row;

/// Accepts a value of *any* Postgres type without decoding it. Used to
/// detect NULL vs. non-NULL for the placeholder fallback in `text_value`
/// without needing a concrete Rust type for every OID (interval, jsonb,
/// bytea, arrays, ranges, ...). `Option<AnyValue>` relies on tokio-postgres
/// short-circuiting NULL columns to `None` before ever calling
/// `AnyValue::from_sql`, so the raw bytes are never actually inspected.
struct AnyValue;

impl<'a> FromSql<'a> for AnyValue {
    fn from_sql(_ty: &Type, _raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(AnyValue)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

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

    /// Uses `try_get` (not `get`) everywhere: `get` panics on a decode
    /// failure, which would kill the streaming task inside the runtime
    /// silently — the channel's tx side drops, the UI sees a clean stream
    /// end, and reports SUCCESS with truncated data. `arrow_type` only maps
    /// BOOL/INT2/INT4/INT8/FLOAT4/FLOAT8 to these typed builders, so the
    /// known decode hazards (NUMERIC 'NaN', 'infinity' timestamps/dates,
    /// out-of-range dates) can't actually reach this method today — they're
    /// all routed to `text_value` via the `Utf8` fallback in `arrow_type`.
    /// The `try_get` + `append_null` here is defense in depth in case that
    /// mapping ever changes.
    pub fn append(&mut self, row: &Row, i: usize) {
        match self {
            Self::Bool(b) => b.append_option(row.try_get::<_, Option<bool>>(i).unwrap_or(None)),
            Self::I16(b) => b.append_option(row.try_get::<_, Option<i16>>(i).unwrap_or(None)),
            Self::I32(b) => b.append_option(row.try_get::<_, Option<i32>>(i).unwrap_or(None)),
            Self::I64(b) => b.append_option(row.try_get::<_, Option<i64>>(i).unwrap_or(None)),
            Self::F32(b) => b.append_option(row.try_get::<_, Option<f32>>(i).unwrap_or(None)),
            Self::F64(b) => b.append_option(row.try_get::<_, Option<f64>>(i).unwrap_or(None)),
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

/// Legal Postgres values exist that the target Rust type can't represent —
/// NUMERIC 'NaN' (`rust_decimal` has no NaN), 'infinity'/'-infinity'
/// timestamps and dates (chrono has no such value), and dates outside
/// chrono's range. `row.get` panics on these, which would silently kill the
/// streaming task; `try_get` turns that into a renderable placeholder
/// instead, so a stream never truncates without surfacing an error.
fn text_value(row: &Row, i: usize) -> Option<String> {
    let t = row.columns()[i].type_();
    let decode_error = || Some(format!("<decode error: {}>", t));
    match *t {
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            match row.try_get::<_, Option<String>>(i) {
                Ok(v) => v,
                Err(_) => decode_error(),
            }
        }
        Type::NUMERIC => match row.try_get::<_, Option<rust_decimal::Decimal>>(i) {
            Ok(v) => v.map(|d| d.to_string()),
            Err(_) => decode_error(),
        },
        Type::TIMESTAMP => match row.try_get::<_, Option<chrono::NaiveDateTime>>(i) {
            Ok(v) => v.map(|v| v.to_string()),
            Err(_) => decode_error(),
        },
        Type::TIMESTAMPTZ => match row.try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(i) {
            Ok(v) => v.map(|v| v.to_rfc3339()),
            Err(_) => decode_error(),
        },
        Type::DATE => match row.try_get::<_, Option<chrono::NaiveDate>>(i) {
            Ok(v) => v.map(|v| v.to_string()),
            Err(_) => decode_error(),
        },
        Type::TIME => match row.try_get::<_, Option<chrono::NaiveTime>>(i) {
            Ok(v) => v.map(|v| v.to_string()),
            Err(_) => decode_error(),
        },
        Type::UUID => match row.try_get::<_, Option<uuid::Uuid>>(i) {
            Ok(v) => v.map(|v| v.to_string()),
            Err(_) => decode_error(),
        },
        _ => match row.try_get::<_, Option<AnyValue>>(i) {
            Ok(v) => v.map(|_| format!("<oid {}>", t.oid())),
            Err(_) => decode_error(),
        },
    }
}
