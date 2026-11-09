//! Benchmarks for the session pump: per-message routing through the
//! assembler/verifier/router, request planning under the pipeline cap, and
//! pipeline bookkeeping micro-costs.

use std::borrow::Cow;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::downloader::{BLOCK_LEN, BlockPlan, PieceField, PieceStrategy};
use magnetite_core::peer_wire::Message;
use magnetite_core::session::{RequestPipeline, SessionPump};
use sha1::{Digest, Sha1};

const DOWNLOAD_PIECES: u32 = 1_024;
const DOWNLOAD_PIECE_LEN: u32 = 32_768;

/// `DOWNLOAD_PIECES` distinct 32 KiB pieces, the download bench's layout.
fn download_layout() -> (Vec<Vec<u8>>, Vec<u8>) {
    let pieces: Vec<Vec<u8>> = (0..DOWNLOAD_PIECES)
        .map(|index| {
            let byte = u8::try_from((index * 7) % 251).unwrap_or(0);
            vec![byte; DOWNLOAD_PIECE_LEN as usize]
        })
        .collect();
    let hashes = pieces
        .iter()
        .map(|piece| Sha1::digest(piece).to_vec())
        .collect::<Vec<_>>()
        .concat();
    (pieces, hashes)
}

/// A pump whose peer claims every piece.
fn seeded_pump(hashes: &[u8]) -> SessionPump<'_> {
    let mut pump = SessionPump::new(
        hashes,
        DOWNLOAD_PIECES,
        DOWNLOAD_PIECE_LEN,
        u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN),
        256,
        250,
        PieceStrategy::RarestFirst,
    )
    .unwrap();
    let mut wire = PieceField::new(DOWNLOAD_PIECES).unwrap();
    for index in 0..DOWNLOAD_PIECES {
        wire.set_have(index).unwrap();
    }
    pump.handle_message(&Message::Bitfield(Cow::Owned(wire.to_bitfield())))
        .unwrap();
    pump
}

fn bench_session(c: &mut Criterion) {
    let (pieces, hashes) = download_layout();

    let mut group = c.benchmark_group("session");

    // Routing a whole download message-by-message: every block passes
    // through the pipeline release, the assembler, and (per piece) the
    // verifier — the exact work the future socket pump pays per message.
    let total = u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN);
    group.throughput(Throughput::Bytes(total));
    group.bench_function("pump/route-download-1024-pieces-32mib", |b| {
        b.iter_batched(
            || seeded_pump(&hashes),
            |mut pump| {
                while let Some(plan) = pump.next_request().unwrap() {
                    let end = (plan.begin + plan.length) as usize;
                    let block = pieces[plan.index as usize][plan.begin as usize..end].to_vec();
                    pump.handle_message(&Message::Piece {
                        index: plan.index,
                        begin: plan.begin,
                        block: Cow::Owned(block),
                    })
                    .unwrap();
                }
                assert!(pump.is_complete());
            },
            BatchSize::LargeInput,
        );
    });

    // Planning one request at the rarest-first ranking: candidate scan over
    // the peer's have-list plus the availability sort.
    group.bench_function("pump/next-request-rarest-first", |b| {
        b.iter_batched(
            || seeded_pump(&hashes),
            |mut pump| pump.next_request().unwrap().is_some(),
            BatchSize::LargeInput,
        );
    });

    // The re-request loop: setup burns one piece with a corrupt block (the
    // pump records it as rejected and the assembler burns it), then the
    // timed path is `retry_rejected` plus the first re-planned request —
    // the per-corruption cost a session pays on top of the routing above.
    group.bench_function("pump/requeue-rejected-piece", |b| {
        b.iter_batched(
            || {
                let mut pump = seeded_pump(&hashes);
                let mut corrupt = pieces[0].clone();
                corrupt[0] = corrupt[0].wrapping_add(1);
                pump.handle_message(&Message::Piece {
                    index: 0,
                    begin: 0,
                    block: Cow::Owned(corrupt[..BLOCK_LEN as usize].to_vec()),
                })
                .unwrap();
                pump.handle_message(&Message::Piece {
                    index: 0,
                    begin: BLOCK_LEN,
                    block: Cow::Owned(corrupt[BLOCK_LEN as usize..].to_vec()),
                })
                .unwrap();
                pump
            },
            |mut pump| {
                let requeued = pump.retry_rejected().unwrap();
                black_box(requeued == [0_u32] && pump.next_request().unwrap().is_some());
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn bench_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline");

    // The record/release round-trip per block: two BTreeMap probes.
    group.throughput(Throughput::Elements(1));
    group.bench_function("pipeline/record-release-block", |b| {
        b.iter(|| {
            let mut pipeline = RequestPipeline::new(250).unwrap();
            for piece in 0..125_u32 {
                let plan = BlockPlan {
                    index: piece,
                    begin: 0,
                    length: black_box(BLOCK_LEN),
                };
                pipeline.record_request(&plan).unwrap();
                pipeline.release(plan.index, plan.begin);
            }
        });
    });

    // The choke eviction over a full 250-deep pipeline.
    group.bench_function("pipeline/choke-evict-250", |b| {
        b.iter(|| {
            let mut pipeline = RequestPipeline::new(250).unwrap();
            for piece in 0..250_u32 {
                pipeline
                    .record_request(&BlockPlan {
                        index: piece,
                        begin: 0,
                        length: BLOCK_LEN,
                    })
                    .unwrap();
            }
            black_box(pipeline.on_choke().len())
        });
    });

    group.finish();
}

criterion_group!(benches, bench_session, bench_pipeline);
criterion_main!(benches);
