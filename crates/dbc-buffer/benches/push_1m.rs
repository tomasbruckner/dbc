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
