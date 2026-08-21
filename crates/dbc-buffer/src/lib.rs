use std::fs::File;
use std::io::BufReader;

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::arrow::ipc::reader::FileReader;
use dbc_core::arrow::ipc::writer::FileWriter;
use dbc_core::arrow::util::display::array_value_to_string;

const DEFAULT_CAP_ROWS: usize = 500_000;

/// A stored batch: kept in memory, or spilled to an Arrow IPC file on disk.
enum Slot {
    Mem(RecordBatch),
    Spilled { file_ix: usize },
}

/// Columnar result storage. `offsets[i]` = number of rows in slots [0, i),
/// so locating a row is a binary search — O(log n) per lookup, O(1) push.
///
/// Once in-memory rows exceed `cap_rows`, further pushed batches are written
/// each to its own Arrow IPC file in a `TempDir` owned by the buffer (deleted
/// on drop) instead of being kept in memory. Reads of spilled slots go
/// through a one-slot cache — the grid reads sequential windows, so one
/// cached batch eliminates nearly all re-reads.
pub struct ResultBuffer {
    schema: SchemaRef,
    slots: Vec<Slot>,
    offsets: Vec<usize>, // len == slots.len() + 1; offsets[0] == 0
    cap_rows: usize,
    mem_rows: usize,
    spill_dir: Option<tempfile::TempDir>,
    spill_files: usize,
    cache: Option<(usize, RecordBatch)>, // (slot index, batch)
}

impl ResultBuffer {
    pub fn new(schema: SchemaRef) -> Self {
        Self::with_cap(schema, DEFAULT_CAP_ROWS)
    }

    pub fn with_cap(schema: SchemaRef, cap_rows: usize) -> Self {
        Self {
            schema,
            slots: Vec::new(),
            offsets: vec![0],
            cap_rows,
            mem_rows: 0,
            spill_dir: None,
            spill_files: 0,
            cache: None,
        }
    }

    pub fn push(&mut self, batch: RecordBatch) {
        let n = batch.num_rows();
        let total = self.offsets.last().copied().unwrap_or(0) + n;
        self.offsets.push(total);
        if self.mem_rows + n <= self.cap_rows {
            self.mem_rows += n;
            self.slots.push(Slot::Mem(batch));
        } else {
            let dir = self
                .spill_dir
                .get_or_insert_with(|| tempfile::tempdir().expect("spill dir"));
            let path = dir.path().join(format!("spill-{}.arrow", self.spill_files));
            let file = File::create(&path).expect("spill file");
            let mut w = FileWriter::try_new(file, &self.schema).expect("ipc writer");
            w.write(&batch).expect("spill write");
            w.finish().expect("spill finish");
            self.slots.push(Slot::Spilled { file_ix: self.spill_files });
            self.spill_files += 1;
        }
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

    /// (slot index, row-within-slot) for an absolute row.
    fn locate(&self, row: usize) -> (usize, usize) {
        let si = self.offsets.partition_point(|&off| off <= row) - 1;
        (si, row - self.offsets[si])
    }

    /// Resolve a slot to a `RecordBatch` reference, reading from disk (via
    /// the one-slot cache) if the slot is spilled.
    fn slot_batch(&mut self, slot_ix: usize) -> &RecordBatch {
        let is_mem = matches!(self.slots[slot_ix], Slot::Mem(_));
        if is_mem {
            let Slot::Mem(b) = &self.slots[slot_ix] else { unreachable!() };
            return b;
        }
        if self.cache.as_ref().map(|(i, _)| *i) != Some(slot_ix) {
            let Slot::Spilled { file_ix } = self.slots[slot_ix] else { unreachable!() };
            let path = self
                .spill_dir
                .as_ref()
                .expect("spill dir exists")
                .path()
                .join(format!("spill-{file_ix}.arrow"));
            let reader =
                FileReader::try_new(BufReader::new(File::open(path).expect("spill open")), None)
                    .expect("ipc reader");
            let batch = reader.into_iter().next().expect("one batch").expect("read batch");
            self.cache = Some((slot_ix, batch));
        }
        &self.cache.as_ref().unwrap().1
    }

    pub fn cell_text(&mut self, row: usize, col: usize) -> String {
        if row >= self.row_count() || col >= self.column_count() {
            return String::new();
        }
        let (si, ri) = self.locate(row);
        let batch = self.slot_batch(si);
        let array = batch.column(col);
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
}
