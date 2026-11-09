//! Benchmarks for the piece verifier: SHA-1 verdicts over assembled piece
//! bytes — the pass path, the mismatch path, and a whole download.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::verify::PieceVerifier;
use sha1::{Digest, Sha1};

const ONE_PIECE: u32 = 4 * 1_048_576;
const DOWNLOAD_PIECES: u32 = 1_024;
const DOWNLOAD_PIECE_LEN: u32 = 32_768;

fn bench_verify(c: &mut Criterion) {
    // The metainfo digest for one 4 MiB piece, laid out as `pieces` carries it.
    let piece = vec![0x5A_u8; ONE_PIECE as usize];
    let mut hashes = Vec::with_capacity(20);
    hashes.extend_from_slice(&Sha1::digest(&piece));

    // One flipped byte: the mismatch path still pays the full SHA-1 pass.
    let mut corrupt = piece.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;

    // 1_024 distinct 32 KiB pieces: a whole download's worth of verdicts.
    let pieces: Vec<Vec<u8>> = (0..DOWNLOAD_PIECES)
        .map(|index| {
            let byte = u8::try_from((index * 7) % 251).unwrap_or(0);
            vec![byte; DOWNLOAD_PIECE_LEN as usize]
        })
        .collect();
    let mut download_hashes = Vec::with_capacity(DOWNLOAD_PIECES as usize * 20);
    for download_piece in &pieces {
        download_hashes.extend_from_slice(&Sha1::digest(download_piece));
    }

    let mut group = c.benchmark_group("verify");

    group.throughput(Throughput::Bytes(u64::from(ONE_PIECE)));
    group.bench_function("verifier/pass-4mib-piece", |b| {
        b.iter_batched(
            || PieceVerifier::new(&hashes, 1).unwrap(),
            |mut verifier| verifier.verify_piece(0, black_box(&piece)).unwrap(),
            BatchSize::LargeInput,
        );
    });

    group.bench_function("verifier/fail-4mib-piece", |b| {
        b.iter_batched(
            || PieceVerifier::new(&hashes, 1).unwrap(),
            |mut verifier| verifier.verify_piece(0, black_box(&corrupt)).unwrap(),
            BatchSize::LargeInput,
        );
    });

    let total = u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN);
    group.throughput(Throughput::Bytes(total));
    group.bench_function("verifier/download-1024-pieces-32mib", |b| {
        b.iter_batched(
            || PieceVerifier::new(&download_hashes, DOWNLOAD_PIECES).unwrap(),
            |mut verifier| {
                for index in 0..DOWNLOAD_PIECES {
                    verifier
                        .verify_piece(index, &pieces[index as usize])
                        .unwrap();
                }
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_verify);
criterion_main!(benches);
