//! Consumer example: a two-peer endgame race, then the leech becomes a
//! seeder.
//!
//! The siblings (`download_pump.rs`, `reject_and_requeue.rs`) drive one pump
//! against one peer. This one wires the swarm-level half of the session
//! remainder: two pumps share one [`EndgameBoard`]. Peer A chases every
//! block at once and its pipeline saturates; the moment no fresh block is
//! left anywhere the board declares the endgame, and peer B — with nothing
//! fresh to ask for — duplicates the scarcest in-flight demand instead, the
//! policy mainstream clients run when a download stalls on its tail. When
//! the slow peer's delayed deliveries finally land on pieces the fast peer
//! already verified, the `is_done` guard keeps the racing duplicates data.
//! Finally the finished pump answers an inbound `Request` through
//! `plan_upload` — the leech becomes a seeder, serving only verified bytes.
//!
//! Run it with spans visible (the new seams carry `tracing` spans, and the
//! error taxonomy speaks `miette` help text):
//!
//! ```console
//! $ cargo run -p magnetite-core --example endgame_swarm
//! $ RUST_LOG=debug cargo run -p magnetite-core --example endgame_swarm
//! ```

use std::borrow::Cow;
use std::collections::BTreeMap;

use magnetite_core::bencode::{self, Value};
use magnetite_core::downloader::{BLOCK_LEN, BlockPlan, PieceField, PieceStrategy};
use magnetite_core::endgame::EndgameBoard;
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
                    Value::Bytes(Cow::Borrowed(b"magnetite-endgame-demo.bin")),
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

/// The payload both peers serve: piece `p`, offset `o` is `(11p + o) % 251`.
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

/// The block bytes a peer serves for `plan`, sliced from the true payload.
fn serve_block(payload: &[u8], piece_length: u32, plan: &BlockPlan) -> Message<'static> {
    let start = plan.index as usize * piece_length as usize + plan.begin as usize;
    Message::Piece {
        index: plan.index,
        begin: plan.begin,
        block: Cow::Owned(payload[start..start + plan.length as usize].to_vec()),
    }
}

/// Feeds one delivery outcome into the ledger and the demo storage. The
/// `is_missing`/`is_done` guards are what keep a racing duplicate delivery
/// data instead of an error: two connections chased the same block, and
/// both may assemble it.
fn absorb(
    name: &str,
    board: &mut EndgameBoard,
    storage: &mut BTreeMap<u32, Vec<u8>>,
    plan: &BlockPlan,
    event: &SessionEvent,
) {
    match event {
        SessionEvent::BlockBuffered { .. } => {
            if board.is_missing(plan.index, plan.begin) {
                board
                    .mark_block(plan.index, plan.begin)
                    .expect("the delivery matches the ledger");
            }
        }
        SessionEvent::PieceVerified { index, bytes } => {
            // The caller always persists what its own verifier accepted —
            // each peer keeps its own copy of every verified piece.
            storage.insert(*index, bytes.clone());
            if board.is_missing(plan.index, plan.begin) {
                board
                    .mark_block(plan.index, plan.begin)
                    .expect("the delivery matches the ledger");
            }
            if board.is_done(*index) {
                println!(
                    "race   : {name}'s delivery of piece {index} landed after another \
                     peer verified it — dropped as data, never an error"
                );
            } else {
                board
                    .mark_piece(*index)
                    .expect("the verdict matches the ledger");
                println!("verify : piece {index} passed on {name}");
            }
        }
        other => unreachable!("the demo peers only serve pieces: {other:?}"),
    }
}

/// The swarm fixture: one shared board and two pumps whose peers claim
/// every piece. Peer A's pipeline holds all four blocks at once.
fn build_swarm(
    hashes: &[u8],
    piece_count: u32,
    piece_length: u32,
    total: u64,
) -> (EndgameBoard, SessionPump<'_>, SessionPump<'_>) {
    let board =
        EndgameBoard::new(piece_count, piece_length, total).expect("the generated layout is valid");
    let mut peer_a = SessionPump::new(
        hashes,
        piece_count,
        piece_length,
        total,
        8,
        4,
        PieceStrategy::Sequential,
    )
    .expect("the generated layout is valid");
    let mut peer_b = SessionPump::new(
        hashes,
        piece_count,
        piece_length,
        total,
        8,
        4,
        PieceStrategy::Sequential,
    )
    .expect("the generated layout is valid");
    let mut wire = PieceField::new(piece_count).expect("two pieces is a valid field");
    for index in 0..piece_count {
        wire.set_have(index).expect("the index is in range");
    }
    let claims = Message::Bitfield(Cow::Owned(wire.to_bitfield()));
    peer_a
        .handle_message(&claims)
        .expect("the generated bitfield is valid");
    peer_b
        .handle_message(&claims)
        .expect("the generated bitfield is valid");
    (board, peer_a, peer_b)
}

/// Phase 4: peer A chases every block; its deliveries are slow (a stalled
/// tail), so its requests sit in flight while its pipeline fills.
fn chase_everything(pump: &mut SessionPump<'_>, board: &mut EndgameBoard) -> Vec<BlockPlan> {
    let mut mailbox = Vec::new();
    while let Some(plan) = pump
        .next_request_endgame(board)
        .expect("the board and the pump agree on the layout")
    {
        mailbox.push(plan);
    }
    println!(
        "peer-a : pipeline saturated — {} blocks in flight, deliveries slow",
        mailbox.len()
    );
    mailbox
}

/// Phase 5: peer B finds nothing fresh anywhere — the endgame — so its
/// planner duplicates the scarcest in-flight demand until the torrent is
/// complete.
fn race_the_tail(
    pump: &mut SessionPump<'_>,
    board: &mut EndgameBoard,
    payload: &[u8],
    piece_length: u32,
    storage: &mut BTreeMap<u32, Vec<u8>>,
) {
    println!("endgame: on (every fresh block is chased); duplicates allowed");
    while !pump.is_complete() {
        let plan = pump
            .next_request_endgame(board)
            .expect("the board and the pump agree on the layout")
            .expect("the chased tail keeps offering duplicates");
        println!(
            "plan   : duplicate piece {} block at {} ({} bytes) -> peer-b",
            plan.index, plan.begin, plan.length
        );
        let message = serve_block(payload, piece_length, &plan);
        let event = pump
            .handle_message(&message)
            .expect("the served block matches the layout");
        absorb("peer-b", board, storage, &plan, &event);
    }
    println!(
        "peer-b : complete — {} verified pieces, board missing={}",
        pump.verified_count(),
        board.missing_blocks()
    );
}

/// Phase 6: the slow peer's delayed deliveries finally arrive — every one
/// of them a racing duplicate of bytes the swarm already verified.
fn flush_slow_peer(
    pump: &mut SessionPump<'_>,
    board: &mut EndgameBoard,
    payload: &[u8],
    piece_length: u32,
    storage: &mut BTreeMap<u32, Vec<u8>>,
    mailbox: Vec<BlockPlan>,
) {
    for plan in mailbox {
        let message = serve_block(payload, piece_length, &plan);
        let event = pump
            .handle_message(&message)
            .expect("the served block matches the layout");
        absorb("peer-a", board, storage, &plan, &event);
    }
    println!(
        "peer-a : complete — {} verified pieces, racing duplicates cost nothing",
        pump.verified_count()
    );
}

/// Phase 7: the leech becomes a seeder. An inbound `Request` hits the
/// finished pump, the upload policy judges it against the verified pieces,
/// and the served bytes come straight out of the caller's storage.
fn seeder_turn(storage: &BTreeMap<u32, Vec<u8>>, seeder: &SessionPump<'_>) {
    match seeder.plan_upload(0, 0, BLOCK_LEN) {
        Ok(decision) => match decision.serve_plan() {
            Some(plan) => {
                let bytes = &storage[&plan.index];
                let start = plan.begin as usize;
                let served = &bytes[start..start + plan.length as usize];
                println!(
                    "serve  : peer-a answers piece {} block at {} with {} verified bytes",
                    plan.index,
                    plan.begin,
                    served.len()
                );
            }
            None => {
                println!("refuse : piece {} is not verified", decision.piece_index());
            }
        },
        Err(error) => println!("error  : {error}"),
    }
    // A fresh leech asking the same question is refused as data: nothing
    // verified yet, nothing served.
    match seeder.plan_upload(1, 0, BLOCK_LEN) {
        Ok(decision) => {
            if decision.serve_plan().is_none() {
                println!(
                    "refuse : piece {} is not verified on this seeder",
                    decision.piece_index()
                );
            }
        }
        Err(error) => println!("error  : {error}"),
    }
}
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 1. A tiny torrent: two 20_000-byte pieces (two blocks each), parsed
    //    with its infohash, and the magnet URI a swarm would hand around.
    let (piece_count, piece_length) = (2_u32, 20_000_u32);
    let content = payload(piece_count, piece_length);
    let total = content.len() as u64;
    let raw = metainfo_bytes(&content);
    let meta = MetaInfo::parse(&raw).expect("the generated metainfo is valid");
    println!(
        "torrent: {} \u{b7} {} bytes \u{d7} {} \u{d7} {} bytes",
        meta.info_hash,
        meta.total_length(),
        meta.piece_count(),
        piece_length,
    );
    println!(
        "magnet : magnet:?xt=urn:btih:{}&dn=magnetite-endgame-demo.bin",
        meta.info_hash
    );

    // 2. The expected digests, borrowed from the metainfo's `pieces` field.
    let hashes = match &meta.info {
        magnetite_core::metainfo::TorrentInfo::Single { pieces, .. }
        | magnetite_core::metainfo::TorrentInfo::Multi { pieces, .. } => pieces.clone(),
    };

    // 3. The swarm: one shared board, two pumps whose peers claim
    //    everything.
    let (mut board, mut peer_a, mut peer_b) =
        build_swarm(&hashes, piece_count, piece_length, total);

    // 4. Peer A chases every block at once; its deliveries are slow.
    let slow_mailbox = chase_everything(&mut peer_a, &mut board);

    // 5. Peer B races the tail through the endgame ledger.
    let mut storage_b: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    race_the_tail(
        &mut peer_b,
        &mut board,
        &content,
        piece_length,
        &mut storage_b,
    );

    // 6. Peer A's slow deliveries arrive — racing duplicates, kept data.
    let mut storage_a: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    flush_slow_peer(
        &mut peer_a,
        &mut board,
        &content,
        piece_length,
        &mut storage_a,
        slow_mailbox,
    );

    // 7. The leech becomes a seeder.
    seeder_turn(&storage_a, &peer_a);
    println!(
        "done   : board missing={}, endgame={}",
        board.missing_blocks(),
        board.in_endgame()
    );
}
