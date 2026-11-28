//! Consumer demo: the storage seam end to end, runtime-free.
//!
//! Run it with `cargo run -p magnetite-core --example verified_storage`.
//! A fake peer offers a three-piece torrent, the session pump routes the
//! delivered blocks through assembly and verification, every
//! [`SessionEvent::PieceVerified`] hand-off lands in a [`MemoryStorage`]
//! backend through the [`Storage`] trait, and the pump's upload policy
//! then serves a verified block straight back out of storage — the exact
//! hand-off the `tokio` stream session performs at the I/O edge, shown
//! here without a socket so every step is visible.

use std::borrow::Cow;

use magnetite_core::downloader::{PieceField, PieceStrategy};
use magnetite_core::error::Result;
use magnetite_core::peer_wire::Message;
use magnetite_core::session::{ServeDecision, SessionEvent, SessionPump};
use magnetite_core::storage::{MemoryStorage, Storage};
use sha1::{Digest, Sha1};

/// Deterministic payload: offset `o` of piece `p` is `(7p + o) % 251` — the
/// same pattern the session specs use.
fn piece_bytes(piece: u32, length: u32) -> Vec<u8> {
    (0..length)
        .map(|offset| u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251).unwrap_or(0))
        .collect()
}

fn main() -> Result<()> {
    const PIECE_LENGTH: u32 = 32_768;
    const BLOCK_LEN: u32 = 16_384;
    let pieces: Vec<Vec<u8>> = (0..3)
        .map(|p| piece_bytes(p, if p == 2 { 20_000 } else { PIECE_LENGTH }))
        .collect();
    let piece_count = u32::try_from(pieces.len()).unwrap_or(0);
    let total: u64 = pieces.iter().map(Vec::len).sum::<usize>() as u64;

    // The metainfo's concatenated per-piece digests, hashed independently
    // of the pump's verifier the way the specs do.
    let mut hashes = Vec::new();
    for piece in &pieces {
        let digest: [u8; 20] = Sha1::digest(piece).into();
        hashes.extend_from_slice(&digest);
    }

    // The runtime-free pump: routing, planning, and verification.
    let mut pump = SessionPump::new(
        &hashes,
        piece_count,
        PIECE_LENGTH,
        total,
        8,
        250,
        PieceStrategy::RarestFirst,
    )?;

    // The peer claims every piece; the pump plans requests block by block.
    let mut wire = PieceField::new(piece_count)?;
    for index in 0..piece_count {
        wire.set_have(index)?;
    }
    let event = pump.handle_message(&Message::Bitfield(Cow::Owned(wire.to_bitfield())))?;
    println!("peer bitfield accepted: {event:?}");

    // 1. Deliver every block; each `PieceVerified` verdict's bytes go
    //    straight into storage through the `Storage` trait — the hand-off
    //    the "pump holds no bytes" contract names.
    let mut storage = MemoryStorage::new();
    'outer: for (index, piece) in pieces.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(0);
        let piece_len = u32::try_from(piece.len()).unwrap_or(u32::MAX);
        let mut begin = 0_u32;
        while begin < piece_len {
            let length = piece_len.saturating_sub(begin).min(BLOCK_LEN);
            let event = pump.handle_message(&Message::Piece {
                index,
                begin,
                block: Cow::Borrowed(&piece[begin as usize..(begin + length) as usize]),
            })?;
            if let SessionEvent::PieceVerified { index, bytes } = event {
                println!("piece {index} verified: {} bytes -> storage", bytes.len());
                storage.write_piece(index, &bytes)?;
                if storage.held_count() == piece_count {
                    break 'outer;
                }
            }
            begin += length;
        }
    }
    println!("storage holds {} piece(s)", storage.held_count());

    // 2. The upload policy judges an inbound request for a verified piece;
    //    the backend answers with the exact block, and a real session
    //    would write it onto the wire.
    match pump.plan_upload(1, 16_384, 16_384)? {
        ServeDecision::Serve { plan } => {
            let bytes = storage
                .read_block(plan.index, plan.begin, plan.length)?
                .expect("verified piece 1 is held");
            println!(
                "serve piece {} @ {}..{}: {} bytes, first byte {:#04x}",
                plan.index,
                plan.begin,
                plan.begin + plan.length,
                bytes.len(),
                bytes[0]
            );
        }
        ServeDecision::PieceNotVerified { .. } => unreachable!("piece 1 verified"),
    }

    // 3. A piece the backend never held reads as `Ok(None)` — data, not an
    //    error: the session simply does not write.
    let missing = storage.read_block(9, 0, BLOCK_LEN)?;
    println!("piece 9 on hand? {}", missing.is_some());

    // 4. The resume query a long-lived client asks between sessions.
    println!(
        "resume state: {} of {piece_count} piece(s) held",
        storage.held_count()
    );
    Ok(())
}
