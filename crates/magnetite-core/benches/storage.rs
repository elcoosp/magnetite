//! Benchmarks for the in-memory storage backend.
//!
//! `write_piece` is the pump's hand-off cost per verified piece (a full
//! 4 MiB piece copy) and `read_block` is the upload serve path's slice
//! cost per 16 KiB block — both are the shapes the session edges drive
//! through the [`Storage`] trait.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::storage::{MemoryStorage, Storage};

/// One 4 MiB piece of deterministic bytes, the shape a real verify pipeline
/// hands over.
fn piece(index: u32) -> Vec<u8> {
    (0..(4 * 1024 * 1024_u64))
        .map(|offset| u8::try_from((u64::from(index) * 7 + offset) % 251).unwrap_or(0))
        .collect()
}

fn bench_storage(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage");
    let piece = piece(0);

    group.throughput(Throughput::Bytes(piece.len() as u64));
    group.bench_function("write-piece/4mib", |b| {
        b.iter_batched(
            MemoryStorage::new,
            |mut storage| {
                black_box(storage.write_piece(0, black_box(&piece))).expect("stored");
            },
            criterion::BatchSize::SmallInput,
        );
    });

    let mut storage = MemoryStorage::new();
    storage.write_piece(0, &piece).expect("seeded");
    group.throughput(Throughput::Bytes(16 * 1024));
    group.bench_function("read-block/16kib", |b| {
        b.iter(|| {
            black_box(
                storage
                    .read_block(0, 0, 16 * 1024)
                    .expect("in-range")
                    .expect("held"),
            );
        });
    });

    group.throughput(Throughput::Bytes(piece.len() as u64));
    group.bench_function("has-piece-probe/4mib-piece", |b| {
        b.iter(|| black_box(storage.has_piece(0)));
    });

    group.finish();
}

criterion_group!(benches, bench_storage);
criterion_main!(benches);
