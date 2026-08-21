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
        _ => row
            .get::<_, Option<AnyValue>>(i)
            .map(|_| format!("<oid {}>", t.oid())),
    }
}
