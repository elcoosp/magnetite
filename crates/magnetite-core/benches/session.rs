//! Benchmarks for the session pump: per-message routing through the
//! assembler/verifier/router, request planning under the pipeline cap,
//! pipeline bookkeeping micro-costs, and the swarm-level endgame ledger
//! (fresh/endgame picks plus the upload serve policy).

use std::borrow::Cow;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::downloader::{BLOCK_LEN, BlockPlan, PieceField, PieceStrategy};
use magnetite_core::endgame::EndgameBoard;
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

fn bench_endgame(c: &mut Criterion) {
    // The swarm-level ledger over the download layout: 1_024 pieces of
    // 32 KiB tile into 2_048 blocks.
    let total = u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN);
    let mut chased = EndgameBoard::new(DOWNLOAD_PIECES, DOWNLOAD_PIECE_LEN, total).unwrap();
    for index in 0..DOWNLOAD_PIECES {
        for begin in [0_u32, BLOCK_LEN] {
            chased
                .record_request(&BlockPlan {
                    index,
                    begin,
                    length: BLOCK_LEN,
                })
                .unwrap();
        }
    }
    assert!(chased.in_endgame());

    let mut group = c.benchmark_group("endgame");

    // The endgame pick over a fully chased tail: the scarcest-demand scan
    // across every missing block of a 1_024-piece torrent.
    group.throughput(Throughput::Elements(1));
    group.bench_function("pick-scarcest-1024-pieces", |b| {
        b.iter(|| black_box(chased.pick_endgame()));
    });

    // The record/release round-trip per block: two BTreeMap probes, the
    // swarm-level twin of the pipeline bench below.
    group.bench_function("record-release-block", |b| {
        b.iter(|| {
            let plan = BlockPlan {
                index: black_box(7),
                begin: 0,
                length: black_box(BLOCK_LEN),
            };
            chased.record_request(&plan).unwrap();
            chased.release_request(plan.index, plan.begin);
        });
    });

    group.finish();
}

fn bench_endgame_pump(c: &mut Criterion) {
    let (_, hashes) = download_layout();

    let mut group = c.benchmark_group("session");

    // One endgame-planned request: the fresh walk finds nothing (every
    // block is chased somewhere), so the pump duplicates the scarcest
    // in-flight demand — the per-request cost a stalled tail pays.
    group.throughput(Throughput::Elements(1));
    group.bench_function("pump/next-request-endgame-duplicate", |b| {
        b.iter_batched(
            || {
                let total = u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN);
                let mut board =
                    EndgameBoard::new(DOWNLOAD_PIECES, DOWNLOAD_PIECE_LEN, total).unwrap();
                for index in 0..DOWNLOAD_PIECES {
                    for begin in [0_u32, BLOCK_LEN] {
                        board
                            .record_request(&BlockPlan {
                                index,
                                begin,
                                length: BLOCK_LEN,
                            })
                            .unwrap();
                    }
                }
                (seeded_pump(&hashes), board)
            },
            |(mut pump, mut board)| {
                let plan = pump.next_request_endgame(&mut board).unwrap();
                black_box(plan.is_some())
            },
            BatchSize::LargeInput,
        );
    });

    // Judging one inbound request through the upload policy on a pump that
    // finished its download: layout checks plus the verified-field probe.
    let small_pieces = 8_u32;
    let small_length = BLOCK_LEN;
    let small_total = u64::from(small_pieces) * u64::from(small_length);
    let small_hashes: Vec<u8> = (0..small_pieces)
        .map(|index| {
            let byte = u8::try_from((index * 11) % 251).unwrap_or(0);
            vec![byte; small_length as usize]
        })
        .collect::<Vec<_>>()
        .iter()
        .flat_map(|piece| Sha1::digest(piece).to_vec())
        .collect();
    let small_payload: Vec<u8> = (0..small_pieces)
        .flat_map(|index| {
            let byte = u8::try_from((index * 11) % 251).unwrap_or(0);
            vec![byte; small_length as usize]
        })
        .collect();
    let mut small_pump = SessionPump::new(
        &small_hashes,
        small_pieces,
        small_length,
        small_total,
        8,
        250,
        PieceStrategy::Sequential,
    )
    .unwrap();
    let mut wire = PieceField::new(small_pieces).unwrap();
    for index in 0..small_pieces {
        wire.set_have(index).unwrap();
    }
    small_pump
        .handle_message(&Message::Bitfield(Cow::Owned(wire.to_bitfield())))
        .unwrap();
    while let Some(plan) = small_pump.next_request().unwrap() {
        let start = plan.index as usize * small_length as usize + plan.begin as usize;
        let end = start + plan.length as usize;
        let block = small_payload[start..end].to_vec();
        small_pump
            .handle_message(&Message::Piece {
                index: plan.index,
                begin: plan.begin,
                block: Cow::Owned(block),
            })
            .unwrap();
    }
    assert!(small_pump.is_complete());
    group.bench_function("pump/plan-upload-verified", |b| {
        b.iter(|| {
            black_box(
                small_pump
                    .plan_upload(3, 0, black_box(BLOCK_LEN))
                    .unwrap()
                    .serve_plan()
                    .is_some(),
            );
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_session,
    bench_pipeline,
    bench_endgame,
    bench_endgame_pump
);
criterion_main!(benches);
