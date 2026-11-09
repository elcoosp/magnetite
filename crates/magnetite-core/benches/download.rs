//! Benchmarks for the piece downloader bookkeeping: bitfield parsing,
//! rarest-first picking, and block assembly throughput.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::downloader::{
    Availability, BlockAssembler, PieceField, PiecePicker, PieceStrategy,
};

const PIECE_COUNT: u32 = 16_384;
const BLOCK_LEN: u32 = 16_384;
const ONE_PIECE: u32 = 4 * 1_048_576;
const DOWNLOAD_PIECES: u32 = 1_024;
const DOWNLOAD_PIECE_LEN: u32 = 32_768;

/// A bitfield for `piece_count` pieces with every third piece marked have.
fn sparse_field(piece_count: u32) -> PieceField {
    let mut field = PieceField::new(piece_count).unwrap();
    let mut index = 0;
    while index < piece_count {
        field.set_have(index).unwrap();
        index += 3;
    }
    field
}

fn bench_download(c: &mut Criterion) {
    let field = sparse_field(PIECE_COUNT);
    let wire = field.to_bitfield();

    let mut group = c.benchmark_group("downloader");

    group.throughput(Throughput::Bytes(wire.len() as u64));
    group.bench_function("bitfield/parse-16384-pieces", |b| {
        b.iter(|| PieceField::from_bitfield(black_box(&wire), black_box(PIECE_COUNT)).unwrap());
    });

    group.bench_function("availability/add-peer-16384-pieces", |b| {
        b.iter_batched(
            || Availability::new(PIECE_COUNT).unwrap(),
            |mut availability| availability.add_peer(black_box(&field)),
            BatchSize::SmallInput,
        );
    });

    // Ours holds nothing yet, the peer has every third piece: a pick scans
    // the peer's field and ranks the candidates by scarcity.
    let ours = PieceField::new(PIECE_COUNT).unwrap();
    let mut availability = Availability::new(PIECE_COUNT).unwrap();
    availability.add_peer(&field);
    let picker = PiecePicker::new(PieceStrategy::RarestFirst);
    group.bench_function("picker/rarest-first-16384-pieces", |b| {
        b.iter(|| {
            picker
                .pick(black_box(&ours), black_box(&field), Some(&availability))
                .unwrap()
        });
    });

    // One 4 MiB piece assembled from 256 full blocks.
    let block = vec![0x5A_u8; BLOCK_LEN as usize];
    group.throughput(Throughput::Bytes(u64::from(ONE_PIECE)));
    group.bench_function("assembler/assemble-4mib-piece", |b| {
        b.iter_batched(
            || BlockAssembler::new(1, ONE_PIECE, u64::from(ONE_PIECE), 8).unwrap(),
            |mut assembler| {
                let mut offset = 0_u32;
                while offset < ONE_PIECE {
                    let length = u32::min(BLOCK_LEN, ONE_PIECE - offset);
                    assembler
                        .push_block(0, offset, &block[..length as usize])
                        .unwrap();
                    offset += BLOCK_LEN;
                }
            },
            BatchSize::LargeInput,
        );
    });

    // A whole 1_024-piece download streamed through the assembler: every
    // iteration files 2 blocks per piece (32 MiB total). Blocks are
    // delivered per piece, so a small in-flight cap suffices.
    let total = u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN);
    let full = vec![0x5A_u8; BLOCK_LEN as usize];
    let tail = vec![0x5A_u8; (DOWNLOAD_PIECE_LEN - BLOCK_LEN) as usize];
    group.throughput(Throughput::Bytes(total));
    group.bench_function("assembler/download-1024-pieces-32mib", |b| {
        b.iter(|| {
            let mut downloader =
                BlockAssembler::new(DOWNLOAD_PIECES, DOWNLOAD_PIECE_LEN, total, 8).unwrap();
            for index in 0..DOWNLOAD_PIECES {
                downloader.push_block(index, BLOCK_LEN, &tail).unwrap();
                downloader.push_block(index, 0, &full).unwrap();
            }
            downloader
        });
    });

    group.finish();
}

criterion_group!(benches, bench_download);
criterion_main!(benches);
