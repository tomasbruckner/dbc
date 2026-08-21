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
