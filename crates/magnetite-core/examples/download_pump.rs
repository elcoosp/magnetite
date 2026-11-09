//! Consumer example: the runtime-free download pump, end to end.
//!
//! This demonstrates the full consumer story: one program that chains every
//! milestone shipped so far — bencode encode/decode, BEP 3 metainfo parsing
//! with raw-slice infohashing, BEP 9 magnet round-trips, and the session
//! pump (request pipeline + message router + block assembler + piece
//! verifier) — without a socket, a runtime, or any I/O beyond printing.
//!
//! Run it with spans visible:
//!
//! ```console
//! $ cargo run -p magnetite-core --example download_pump
//! $ RUST_LOG=debug cargo run -p magnetite-core --example download_pump
//! ```

use std::borrow::Cow;
use std::collections::BTreeMap;

use magnetite_core::bencode::{self, Value};
use magnetite_core::downloader::{BlockPlan, PieceField, PieceStrategy};
use magnetite_core::magnet as magnet_lib;
use magnetite_core::metainfo::MetaInfo;
use magnetite_core::peer_wire::Message;
use magnetite_core::session::{SessionEvent, SessionPump};
use sha1::{Digest, Sha1};

/// Bencode-encoded BEP 3 metainfo for a two-piece, 40_000-byte torrent.
fn metainfo_bytes(content: &[u8]) -> Vec<u8> {
    let piece_length = 20_000_u32;
    let mut pieces = Vec::new();
    for chunk in content.chunks(piece_length as usize) {
        let digest: [u8; 20] = Sha1::digest(chunk).into();
        pieces.extend_from_slice(&digest);
    }
    let value = Value::Dict(BTreeMap::from([
        (
            Cow::Borrowed(b"announce".as_slice()),
            Value::Bytes(Cow::Borrowed(b"udp://tracker.example:1337/announce")),
        ),
        (
            Cow::Borrowed(b"info".as_slice()),
            Value::Dict(BTreeMap::from([
                (
                    Cow::Borrowed(b"length".as_slice()),
                    Value::Int(i64::try_from(content.len()).unwrap_or(i64::MAX)),
                ),
                (
                    Cow::Borrowed(b"name".as_slice()),
                    Value::Bytes(Cow::Borrowed(b"magnetite-demo.bin")),
                ),
                (
                    Cow::Borrowed(b"piece length".as_slice()),
                    Value::Int(i64::from(piece_length)),
                ),
                (
                    Cow::Borrowed(b"pieces".as_slice()),
                    Value::Bytes(Cow::Owned(pieces)),
                ),
            ])),
        ),
    ]));
    bencode::encode_to_vec(&value).unwrap_or_default()
}

/// The payload a fake peer serves: piece `p`, offset `o` is `(11p + o) % 251`.
fn payload(piece_count: u32, piece_length: u32) -> Vec<u8> {
    let total = u64::from(piece_count) * u64::from(piece_length);
    (0..total)
        .map(|position| {
            let piece = position / u64::from(piece_length);
            let offset = position % u64::from(piece_length);
            u8::try_from((11 * piece + offset) % 251).unwrap_or(0)
        })
        .collect()
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 1. Build a tiny torrent in memory, parse it, and take its infohash.
    let (piece_count, piece_length) = (2_u32, 20_000_u32);
    let content = payload(piece_count, piece_length);
    let total = content.len() as u64;
    let raw = metainfo_bytes(&content);
    let meta = MetaInfo::parse(&raw).expect("the generated metainfo is valid");
    println!(
        "torrent: {} · {} bytes · {} × {} bytes",
        meta.info_hash,
        meta.total_length(),
        meta.piece_count(),
        piece_length,
    );

    // 2. The magnet URI a swarm would hand around, round-tripped.
    let uri = format!(
        "magnet:?xt=urn:btih:{}&dn=magnetite-demo.bin",
        meta.info_hash
    );
    let magnet = magnet_lib::parse(&uri).expect("the generated magnet URI is valid");
    println!("magnet : {magnet}");

    // 3. The expected digests, borrowed from the metainfo's `pieces` field.
    let hashes = match &meta.info {
        magnetite_core::metainfo::TorrentInfo::Single { pieces, .. }
        | magnetite_core::metainfo::TorrentInfo::Multi { pieces, .. } => pieces.clone(),
    };

    // 4. One peer that claims everything; the pump plans, routes, verifies.
    let mut pump = SessionPump::new(
        &hashes,
        piece_count,
        piece_length,
        total,
        8,
        250,
        PieceStrategy::RarestFirst,
    )
    .expect("the generated layout is valid");
    let mut wire = PieceField::new(piece_count).expect("two pieces is a valid field");
    for index in 0..piece_count {
        wire.set_have(index).expect("the index is in range");
    }
    pump.handle_message(&Message::Bitfield(Cow::Owned(wire.to_bitfield())))
        .expect("the generated bitfield is valid");

    let mut verified = 0_u32;
    let mut buffered = 0_u32;
    while let Some(plan) = pump
        .next_request()
        .expect("the pump plans under its own cap")
    {
        // The fake peer answers every request from the payload, in whatever
        // order the pump planned it.
        let BlockPlan {
            index,
            begin,
            length,
        } = plan;
        let end = (begin + length) as usize;
        let piece_start = index as usize * piece_length as usize;
        let block = content[piece_start + begin as usize..piece_start + end].to_vec();
        let event = pump
            .handle_message(&Message::Piece {
                index,
                begin,
                block: Cow::Owned(block),
            })
            .expect("the served block matches the layout");
        match event {
            SessionEvent::PieceVerified { index, bytes } => {
                verified += 1;
                tracing::info!(index, bytes = bytes.len(), "piece verified");
            }
            SessionEvent::BlockBuffered { index, pending } => {
                buffered += 1;
                tracing::info!(index, pending, "block buffered");
            }
            other => unreachable!("the fake peer serves valid bytes: {other:?}"),
        }
    }

    println!(
        "done   : {verified} pieces verified, {buffered} blocks routed, complete={}",
        pump.is_complete()
    );
}
