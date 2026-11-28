//! Specs for the storage seam (the M3 groundwork).
//!
//! The first spec pins the plan's acceptance criterion verbatim:
//! `ARCHITECTURE.md` §9, the **M3** row:
//!
//! > **M3** | Storage abstraction (multi-file, sparse, resume) | `Storage`
//! > trait so embedding apps can back it with anything; verify pipeline
//! > streams pieces without re-reading.
//!
//! The groundwork drop lands the `Storage` trait plus an in-memory backend,
//! and wires the trait into the `tokio` stream session's upload edge, which
//! until now took a bare `lookup` closure (the seam §4.8 explicitly left
//! open: "the pump holds no bytes; storage stays M3").

use magnetite_core::error::Error;
use magnetite_core::storage::{MemoryStorage, Storage};
use magnetite_core::verify::PieceVerifier;
use sha1::{Digest, Sha1};

/// The two-piece, 40_000-byte layout every session spec uses.
const PIECE_COUNT: u32 = 2;
const PIECE_LENGTH: u32 = 20_000;

/// Deterministic payload: offset `o` of piece `p` is `(7p + o) % 251`.
fn piece_bytes(piece: u32, length: u32) -> Vec<u8> {
    (0..length)
        .map(|offset| u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251).unwrap_or(0))
        .collect()
}

/// Hashes pieces with an independent SHA-1 pass.
fn hashes_for(pieces: &[Vec<u8>]) -> Vec<u8> {
    let mut hashes = Vec::new();
    for piece in pieces {
        let digest: [u8; 20] = Sha1::digest(piece).into();
        hashes.extend_from_slice(&digest);
    }
    hashes
}

/// Plan-fidelity pin: verified pieces land in `MemoryStorage` through the
/// `Storage` trait, and `read_block` slices exactly the block the upload
/// policy names — the "pump holds no bytes" contract with a real backend
/// behind it.
#[test]
fn verified_pieces_round_trip_through_memory_storage() {
    let pieces = vec![piece_bytes(0, PIECE_LENGTH), piece_bytes(1, PIECE_LENGTH)];
    let hashes = hashes_for(&pieces);
    let mut verifier = PieceVerifier::new(&hashes, PIECE_COUNT).expect("layout is consistent");

    let mut storage = MemoryStorage::new();
    assert!(!storage.has_piece(0));
    assert_eq!(storage.held_count(), 0);

    // Verify each piece, then hand it to storage the way the session pump
    // hands `PieceVerified` bytes to the caller's backend.
    for (index, piece) in pieces.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(0);
        let verdict = verifier
            .verify_piece(index, piece)
            .expect("the true bytes verify");
        assert!(verdict, "piece {index} matches its digest");
        storage
            .write_piece(index, piece)
            .expect("verified bytes are stored");
    }
    assert_eq!(storage.held_count(), PIECE_COUNT);
    assert!(storage.has_piece(0));
    assert!(storage.has_piece(1));

    // Re-writing a held piece is a typed error: the verifier never
    // re-verifies, so a duplicate is an assembly bug, not normal flow.
    let err = storage
        .write_piece(0, &pieces[0])
        .expect_err("a duplicate write is a typed error");
    assert!(matches!(err, Error::Download(_)), "got {err}");

    // read_block slices the exact block the upload policy names.
    let block = storage
        .read_block(0, 0, 16_384)
        .expect("in-range reads are fine")
        .expect("piece 0 is held");
    assert_eq!(block, pieces[0][0..16_384]);

    // A missing piece is `Ok(None)` — data, not an error, mirroring the
    // upload policy's refusal shape.
    let missing = storage
        .read_block(7, 0, 16_384)
        .expect("missing pieces read as None");
    assert!(missing.is_none());

    // An out-of-range slice is a typed error, adversarial input as data.
    let err = storage
        .read_block(0, 19_999, 2)
        .expect_err("a slice past the piece end is rejected");
    assert!(matches!(err, Error::Download(_)), "got {err}");
}

/// The two-piece layout for the stream seam spec: piece 0 is downloaded
/// through the pump (so the upload policy has a verified piece), piece 1
/// stays unverified for the refusal case.
#[cfg(feature = "stream")]
#[tokio::test]
async fn stream_session_serves_inbound_requests_from_storage() {
    use magnetite_core::downloader::PieceStrategy;
    use magnetite_core::infohash::InfoHash;
    use magnetite_core::peer_wire::Message;
    use magnetite_core::session_stream::{StepEvent, StreamConfig, StreamSession};

    const PIECES: u32 = 2;
    const PIECE_LEN: u32 = 16_384;
    let piece0 = piece_bytes(0, PIECE_LEN);
    let piece1 = piece_bytes(1, PIECE_LEN);
    let hashes: &'static [u8] =
        Box::leak(hashes_for(&[piece0.clone(), piece1.clone()]).into_boxed_slice());
    let info_hash = InfoHash::from_bytes([0xAB; 20]);

    // Storage pre-filled the way a completed download would have left it.
    let mut storage = MemoryStorage::new();
    storage
        .write_piece(0, &piece0)
        .expect("verified bytes are stored");
    storage
        .write_piece(1, &piece1)
        .expect("verified bytes are stored");

    // The wire bytes the seeder must receive, built from the reference
    // copy before the piece moves into the peer task.
    let mut expected_wire = Vec::new();
    Message::Piece {
        index: 0,
        begin: 0,
        block: std::borrow::Cow::Owned(piece0.clone()),
    }
    .encode_into(&mut expected_wire);

    let (peer_tx, session_rx) = tokio::io::duplex(8 * 1024);
    let (session_tx, peer_rx) = tokio::io::duplex(8 * 1024);
    let peer = spawn_seeder_peer(peer_tx, peer_rx, info_hash, piece0, PIECES, PIECE_LEN);

    let config = StreamConfig::new(hashes, PIECES, PIECE_LEN, u64::from(PIECES * PIECE_LEN))
        .with_strategy(PieceStrategy::Sequential);
    let mut session = StreamSession::handshake(
        session_rx,
        session_tx,
        info_hash,
        *b"-qB4550-magnetite-x!",
        &config,
    )
    .await
    .expect("handshake completes");

    // Step 1 routes the bitfield and drains the first request; step 2
    // delivers the block, verifies piece 0, and finds nothing left to plan;
    // step 3 reads the peer's inbound request and serves it from storage.
    for _ in 0..2 {
        session
            .step_with_storage(&mut storage)
            .await
            .expect("the step pumps the routed frame");
    }
    assert!(session.pump().verified_count() >= 1, "piece 0 verified");

    let step = session
        .step_with_storage(&mut storage)
        .await
        .expect("the step serves from storage");
    match step {
        StepEvent::UploadJudged { index, served } => {
            assert_eq!(index, 0, "the event names the served piece");
            assert!(served, "verified piece 0 is served from storage");
        }
        other => panic!("expected an UploadJudged event, got {other:?}"),
    }

    // The peer received the exact wire bytes storage holds: the Piece
    // reply for piece 0's only block, re-encoded from the backend's bytes.
    let served_wire = peer.await.unwrap();
    assert_eq!(
        served_wire, expected_wire,
        "the served block matches what storage holds"
    );

    // Piece 1 is in the layout but the pump never verified it, so the
    // upload policy refuses it as data — storage contents alone do not
    // override the policy.
    let served = session
        .serve_with_storage(1, 0, PIECE_LEN, &mut storage)
        .await
        .expect("a refusal is data, not an error");
    assert!(!served);
    assert!(!session.is_complete(), "piece 1 never arrived");
}

/// The scripted seeder peer: it handshakes, claims only piece 0, answers
/// the session's one request with the true bytes, then sends its own
/// inbound request and reads back the served block's raw wire form.
#[cfg(feature = "stream")]
fn spawn_seeder_peer(
    mut peer_tx: tokio::io::DuplexStream,
    mut peer_rx: tokio::io::DuplexStream,
    info_hash: magnetite_core::infohash::InfoHash,
    piece0: Vec<u8>,
    piece_count: u32,
    piece_len: u32,
) -> tokio::task::JoinHandle<Vec<u8>> {
    use magnetite_core::downloader::PieceField;
    use magnetite_core::peer_wire::{Handshake, Message};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::spawn(async move {
        let mut hs = [0_u8; 68];
        peer_rx.read_exact(&mut hs).await.unwrap();
        let ours = Handshake::new(info_hash, *b"-qB4550-magnetite-y!");
        peer_tx.write_all(&ours.encode()).await.unwrap();
        let mut wire = PieceField::new(piece_count).unwrap();
        wire.set_have(0).unwrap();
        let mut frame = Vec::new();
        Message::Bitfield(std::borrow::Cow::Owned(wire.to_bitfield())).encode_into(&mut frame);
        peer_tx.write_all(&frame).await.unwrap();

        // Answer the session's request for piece 0's only block.
        let mut req = [0_u8; 17];
        peer_rx.read_exact(&mut req).await.unwrap();
        let mut reply = Vec::new();
        Message::Piece {
            index: 0,
            begin: 0,
            block: std::borrow::Cow::Owned(piece0),
        }
        .encode_into(&mut reply);
        peer_tx.write_all(&reply).await.unwrap();

        // Ask for the block back: the session must serve it from storage.
        let mut ask = Vec::new();
        Message::Request {
            index: 0,
            begin: 0,
            length: piece_len,
        }
        .encode_into(&mut ask);
        peer_tx.write_all(&ask).await.unwrap();

        let mut served = vec![0_u8; 4 + 1 + 8 + piece_len as usize];
        peer_rx.read_exact(&mut served).await.unwrap();
        served
    })
}
