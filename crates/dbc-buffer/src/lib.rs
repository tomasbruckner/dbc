use std::fs::File;
use std::io::BufReader;

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::arrow::ipc::reader::FileReader;
use dbc_core::arrow::ipc::writer::FileWriter;
use dbc_core::arrow::util::display::array_value_to_string;

const DEFAULT_CAP_ROWS: usize = 500_000;
/// Spec §4: "500k rows / 256 MB" — the byte half of that cap. Bounds how much
/// of the in-memory portion of the buffer can grow to before further batches
/// are spilled to disk, independent of the row cap (wide rows can blow past a
/// row-only cap well before 256 MB).
pub const DEFAULT_CAP_BYTES: usize = 256 * 1024 * 1024;

/// Error from a fallible buffer operation (currently: spill I/O on `push`).
/// Kept as an owned message rather than wrapping the underlying `io`/`arrow`
/// error types so callers (the UI status bar) can just `format!("{e}")` it.
#[derive(Debug)]
pub struct BufferError {
    pub message: String,
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BufferError {}

/// A stored batch: kept in memory, or spilled to an Arrow IPC file on disk.
enum Slot {
    Mem(RecordBatch),
    Spilled { file_ix: usize },
}

/// Columnar result storage. `offsets[i]` = number of rows in slots [0, i),
/// so locating a row is a binary search — O(log n) per lookup, O(1) push.
///
/// Once in-memory rows exceed `cap_rows` OR in-memory bytes exceed
/// `cap_bytes`, further pushed batches are written each to its own Arrow IPC
/// file in a `TempDir` owned by the buffer (deleted on drop) instead of being
/// kept in memory. Reads of spilled slots go through a one-slot cache — the
/// grid reads sequential windows, so one cached batch eliminates nearly all
/// re-reads.
///
/// Spill I/O (write on `push`, read on `slot_batch`) is fallible rather than
/// panicking: disk-full / AV-locked temp files must not crash the UI thread
/// that drives both paths (phase 3 follow-up I2). Write failures propagate
/// out of `push`; read failures degrade a single cell to a visible
/// `"<spill read error>"` string instead of taking down the app.
pub struct ResultBuffer {
    schema: SchemaRef,
    slots: Vec<Slot>,
    offsets: Vec<usize>, // len == slots.len() + 1; offsets[0] == 0
    cap_rows: usize,
    cap_bytes: usize,
    mem_rows: usize,
    mem_bytes: usize,
    spill_dir: Option<tempfile::TempDir>,
    spill_files: usize,
    cache: Option<(usize, RecordBatch)>, // (slot index, batch)
}

impl ResultBuffer {
    pub fn new(schema: SchemaRef) -> Self {
        Self::with_caps(schema, DEFAULT_CAP_ROWS, DEFAULT_CAP_BYTES)
    }

    pub fn with_cap(schema: SchemaRef, cap_rows: usize) -> Self {
        Self::with_caps(schema, cap_rows, DEFAULT_CAP_BYTES)
    }

    pub fn with_caps(schema: SchemaRef, cap_rows: usize, cap_bytes: usize) -> Self {
        Self {
            schema,
            slots: Vec::new(),
            offsets: vec![0],
            cap_rows,
            cap_bytes,
            mem_rows: 0,
            mem_bytes: 0,
            spill_dir: None,
            spill_files: 0,
            cache: None,
        }
    }

    /// Appends a batch, keeping it in memory if both caps still allow it,
    /// otherwise spilling it to disk. Spilled batches leave `mem_bytes`
    /// untouched — the caps bound what stays resident, not total buffered
    /// size.
    pub fn push(&mut self, batch: RecordBatch) -> Result<(), BufferError> {
        let n = batch.num_rows();
        let batch_bytes = batch.get_array_memory_size();
        let fits =
            self.mem_rows + n <= self.cap_rows && self.mem_bytes + batch_bytes <= self.cap_bytes;
        if fits {
            self.mem_rows += n;
            self.mem_bytes += batch_bytes;
            self.slots.push(Slot::Mem(batch));
        } else {
            if self.spill_dir.is_none() {
                let dir = tempfile::tempdir()
                    .map_err(|e| BufferError { message: format!("spill dir: {e}") })?;
                self.spill_dir = Some(dir);
            }
            let file_ix = self.spill_files;
            let path =
                self.spill_dir.as_ref().unwrap().path().join(format!("spill-{file_ix}.arrow"));
            let file = File::create(&path)
                .map_err(|e| BufferError { message: format!("spill file: {e}") })?;
            let mut w = FileWriter::try_new(file, &self.schema)
                .map_err(|e| BufferError { message: format!("spill ipc writer: {e}") })?;
            w.write(&batch)
                .map_err(|e| BufferError { message: format!("spill write: {e}") })?;
            w.finish().map_err(|e| BufferError { message: format!("spill finish: {e}") })?;
            self.slots.push(Slot::Spilled { file_ix });
            self.spill_files += 1;
        }
        // Only commit the row index once the slot itself was committed, so a
        // failed push (return above) leaves `offsets.len() == slots.len() + 1`.
        let total = self.offsets.last().copied().unwrap_or(0) + n;
        self.offsets.push(total);
        Ok(())
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
    /// the one-slot cache) if the slot is spilled. Returns `None` if the
    /// spilled file can no longer be read (deleted out from under us,
    /// disk error, corrupt IPC framing, ...) rather than panicking.
    fn slot_batch(&mut self, slot_ix: usize) -> Option<&RecordBatch> {
        let is_mem = matches!(self.slots[slot_ix], Slot::Mem(_));
        if is_mem {
            let Slot::Mem(b) = &self.slots[slot_ix] else { unreachable!() };
            return Some(b);
        }
        if self.cache.as_ref().map(|(i, _)| *i) != Some(slot_ix) {
            let Slot::Spilled { file_ix } = self.slots[slot_ix] else { unreachable!() };
            let dir = self.spill_dir.as_ref()?;
            let path = dir.path().join(format!("spill-{file_ix}.arrow"));
            let file = File::open(path).ok()?;
            let reader = FileReader::try_new(BufReader::new(file), None).ok()?;
            let batch = reader.into_iter().next()?.ok()?;
            self.cache = Some((slot_ix, batch));
        }
        Some(&self.cache.as_ref().unwrap().1)
    }

    pub fn cell_text(&mut self, row: usize, col: usize) -> String {
        if row >= self.row_count() || col >= self.column_count() {
            return String::new();
        }
        let (si, ri) = self.locate(row);
        let Some(batch) = self.slot_batch(si) else {
            return "<spill read error>".to_string();
        };
        let array = batch.column(col);
        if array.is_null(ri) {
            return String::new();
        }
        array_value_to_string(array, ri).unwrap_or_default()
    }

    #[cfg(test)]
    fn spilled_slots(&self) -> usize {
        self.slots.iter().filter(|s| matches!(s, Slot::Spilled { .. })).count()
    }

    #[cfg(test)]
    fn spill_dir(&self) -> Option<&std::path::Path> {
        self.spill_dir.as_ref().map(|d| d.path())
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
        buf.push(b0).unwrap();
        buf.push(batch(100, 50)).unwrap();
        buf.push(batch(150, 25)).unwrap();
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
        buf.push(b).unwrap();
        assert_eq!(buf.cell_text(0, 1), ""); // i % 7 == 0 → None
    }

    #[test]
    fn spills_past_cap_and_reads_back() {
        let b0 = batch(0, 100);
        let mut buf = ResultBuffer::with_cap(b0.schema(), 150); // cap at 150 rows
        buf.push(b0).unwrap();                 // 100 mem
        buf.push(batch(100, 100)).unwrap();    // 200 total → this batch spills
        buf.push(batch(200, 100)).unwrap();    // spills
        assert_eq!(buf.row_count(), 300);
        assert_eq!(buf.cell_text(50, 0), "50");    // mem
        assert_eq!(buf.cell_text(150, 0), "150");  // spilled
        assert_eq!(buf.cell_text(299, 0), "299");  // spilled, different file
        assert_eq!(buf.cell_text(150, 1), "row150");
        // cache flip-flop: read across files repeatedly
        assert_eq!(buf.cell_text(299, 0), "299");
        assert_eq!(buf.cell_text(150, 0), "150");
    }

    #[test]
    fn byte_cap_triggers_spill() {
        let b0 = batch(0, 100);
        // Tiny byte cap (well under one batch's array memory size), generous
        // row cap — so it's the byte cap, not the row cap, forcing the spill.
        let mut buf = ResultBuffer::with_caps(b0.schema(), 1_000_000, 64);
        buf.push(b0).unwrap();
        assert_eq!(buf.spilled_slots(), 1, "first batch already exceeds cap_bytes");
        buf.push(batch(100, 100)).unwrap();
        assert_eq!(buf.spilled_slots(), 2);
        assert_eq!(buf.row_count(), 200);
        assert_eq!(buf.cell_text(0, 0), "0");
        assert_eq!(buf.cell_text(150, 0), "150");
    }

    #[test]
    fn spill_read_error_falls_back_without_panicking() {
        let b0 = batch(0, 10);
        let mut buf = ResultBuffer::with_cap(b0.schema(), 0); // cap at 0 rows: everything spills
        buf.push(b0).unwrap();
        let dir = buf.spill_dir().expect("spill dir created").to_path_buf();
        std::fs::remove_file(dir.join("spill-0.arrow")).unwrap();
        assert_eq!(buf.cell_text(0, 0), "<spill read error>");
        assert_eq!(buf.cell_text(5, 1), "<spill read error>");
    }
}
