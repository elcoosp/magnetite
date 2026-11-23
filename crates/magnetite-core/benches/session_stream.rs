//! Benchmarks for the `tokio` I/O edge of the session: the per-message
//! routing overhead the stream reader adds on top of the runtime-free
//! [`SessionPump`], measured against the same 1_024-piece, 32 MiB download
//! the `session` bench routes synchronously.
//!
//! Gated behind the `stream` feature: the bench needs `tokio`, which the
//! runtime-free core does not otherwise pull in.

#![cfg(feature = "stream")]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::downloader::{BLOCK_LEN, PieceField, PieceStrategy};
use magnetite_core::peer_wire::{Handshake, Message};
use magnetite_core::session_stream::{StreamConfig, StreamSession};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DOWNLOAD_PIECES: u32 = 1_024;
const DOWNLOAD_PIECE_LEN: u32 = 32_768;

/// `DOWNLOAD_PIECES` distinct 32 KiB pieces, the same layout the `session`
/// bench routes synchronously — so the two throughput numbers are directly
/// comparable: the gap is the I/O edge's per-message cost.
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

/// A fake peer that drives the writer half of a duplex: reads and discards
/// the session's handshake, sends its own handshake and a full bitfield,
/// then answers every `Request` the session writes with the true block.
async fn drive_peer(
    mut rx: tokio::io::DuplexStream,
    mut tx: tokio::io::DuplexStream,
    info_hash: magnetite_core::infohash::InfoHash,
    payload: Vec<u8>,
    piece_length: u32,
    expected_requests: u32,
) {
    // Discard the session's handshake.
    let mut their_hs = [0_u8; 68];
    rx.read_exact(&mut their_hs).await.unwrap();
    // Send ours.
    let hs = Handshake::new(info_hash, *b"-qB4550-magnetite-y!");
    tx.write_all(&hs.encode()).await.unwrap();
    // Send a full bitfield.
    let mut wire = PieceField::new(DOWNLOAD_PIECES).unwrap();
    for index in 0..DOWNLOAD_PIECES {
        wire.set_have(index).unwrap();
    }
    let mut frame = Vec::new();
    Message::Bitfield(std::borrow::Cow::Owned(wire.to_bitfield())).encode_into(&mut frame);
    tx.write_all(&frame).await.unwrap();
    // Answer every request.
    let mut buf = [0_u8; 17];
    for _ in 0..expected_requests {
        rx.read_exact(&mut buf).await.unwrap();
        let plan = match magnetite_core::peer_wire::parse_message(&buf).unwrap() {
            Message::Request {
                index,
                begin,
                length,
            } => magnetite_core::downloader::BlockPlan {
                index,
                begin,
                length,
            },
            _ => unreachable!("the session only writes Requests"),
        };
        let start = plan.index as usize * piece_length as usize + plan.begin as usize;
        let end = start + plan.length as usize;
        let block = payload[start..end].to_vec();
        let mut frame = Vec::new();
        Message::Piece {
            index: plan.index,
            begin: plan.begin,
            block: std::borrow::Cow::Owned(block),
        }
        .encode_into(&mut frame);
        tx.write_all(&frame).await.unwrap();
    }
}

/// Routes a whole 1_024-piece, 32 MiB download through the async stream
/// session: the per-message cost the I/O edge adds on top of the
/// runtime-free `SessionPump` (read → `next_message` → `into_owned` →
/// `handle_message` → `next_request` → write).
fn bench_stream_session(c: &mut Criterion) {
    let (pieces, hashes) = download_layout();
    // `leak` lifts the hashes to `'static` so the session's borrowed
    // verifier outlives the bench iteration — the same lifetime trick the
    // test module uses.
    let hashes: &'static [u8] = Box::leak(hashes.into_boxed_slice());
    let payload: Vec<u8> = pieces.iter().flatten().copied().collect();
    let info_hash = magnetite_core::infohash::InfoHash::from_bytes([0xAB; 20]);
    let total = u64::from(DOWNLOAD_PIECES) * u64::from(DOWNLOAD_PIECE_LEN);
    let blocks_per_piece = DOWNLOAD_PIECE_LEN / BLOCK_LEN;
    let expected_requests = DOWNLOAD_PIECES * blocks_per_piece;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("session_stream");
    group.throughput(Throughput::Bytes(total));
    group.bench_function("stream/route-download-1024-pieces-32mib", |b| {
        b.iter_batched(
            || {
                let (peer_tx, session_rx) = tokio::io::duplex(64 * 1024);
                let (session_tx, peer_rx) = tokio::io::duplex(64 * 1024);
                let driver = rt.spawn(drive_peer(
                    peer_rx,
                    peer_tx,
                    info_hash,
                    payload.clone(),
                    DOWNLOAD_PIECE_LEN,
                    expected_requests,
                ));
                let config = StreamConfig::new(hashes, DOWNLOAD_PIECES, DOWNLOAD_PIECE_LEN, total)
                    .with_strategy(PieceStrategy::Sequential);
                let session = rt
                    .block_on(StreamSession::handshake(
                        session_rx,
                        session_tx,
                        info_hash,
                        *b"-qB4550-magnetite-x!",
                        &config,
                    ))
                    .unwrap();
                (session, driver)
            },
            |(mut session, driver)| {
                rt.block_on(async {
                    while !session.is_complete() {
                        session
                            .step(|_, _, _| None)
                            .await
                            .expect("the step pumps the routed frame");
                    }
                });
                rt.block_on(driver).unwrap();
                black_box(session.is_complete());
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_stream_session);
criterion_main!(benches);
