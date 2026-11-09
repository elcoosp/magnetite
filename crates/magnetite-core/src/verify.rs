//! Runtime-free piece verification: SHA-1 checking of assembled pieces
//! against the metainfo's per-piece hashes.
//!
//! This closes the loop the downloader opens: [`crate::downloader::BlockAssembler`]
//! emits each piece's bytes exactly once "for the caller's SHA-1
//! verification" — and this module is that caller. It turns
//! `crates/magnetite-core/src/metainfo.rs`'s `pieces` field (concatenated
//! 20-byte SHA-1 digests, one per piece) into a verdict per assembled piece
//! and a [`PieceField`] of verified pieces the future session can act on:
//! re-request what failed, announce what passed, and only ever hand
//! verified bytes to storage.
//!
//! The session pump the drop enables, entirely runtime-free:
//!
//! ```no_run
//! use magnetite_core::downloader::BlockAssembler;
//! use magnetite_core::error::Error;
//! use magnetite_core::metainfo::MetaInfo;
//! use magnetite_core::peer_wire::Message;
//! use magnetite_core::verify::PieceVerifier;
//!
//! # fn route(msg: &Message<'static>) -> Result<(), Error> { Ok(()) }
//! # fn main() -> Result<(), Error> {
//! # let raw = b"";
//! # let msg = Message::KeepAlive;
//! let meta = MetaInfo::parse(raw)?;
//! let (piece_count, piece_length) = (12_u32, 262_144_u32);
//! let total = u64::from(piece_count) * u64::from(piece_length);
//! let mut assembler = BlockAssembler::new(piece_count, piece_length, total, 8)?;
//! let hashes = match &meta.info {
//!     magnetite_core::metainfo::TorrentInfo::Single { pieces, .. }
//!     | magnetite_core::metainfo::TorrentInfo::Multi { pieces, .. } => pieces.as_slice(),
//! };
//! let mut verifier = PieceVerifier::new(hashes, piece_count)?;
//!
//! // Per incoming wire message, the pump:
//! # route(&msg)?;
//! if let Some((index, piece)) = assembler.push_message(&msg)? {
//!     if !verifier.verify_piece(index, &piece)? {
//!         // Mismatch is data, not a crash: re-request the piece elsewhere.
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! House rules, unchanged from the codec, tracker, peer wire, and downloader
//! edges:
//!
//! - Adversarial input is *data*: a piece whose bytes hash differently is an
//!   `Ok(false)` verdict — the normal fate of bytes from a hostile or
//!   broken peer — while protocol-shape violations (out-of-range or
//!   already-verified piece indexes, a hash slice that is not a multiple of
//!   20 bytes, a count mismatch against the metainfo) are typed
//!   [`Error::Download`] values. Nothing panics.
//! - Byte strings stay borrowed: the expected digests are borrowed from the
//!   caller's metainfo (`PieceVerifier<'a>`), exactly like the decoder's
//!   `Cow` values — the verifier copies nothing.
//! - The module is runtime-free and adds no dependencies; `sha1` is already
//!   in the tree for the infohash.

use sha1::{Digest, Sha1};

use crate::downloader::PieceField;
use crate::error::{Error, Result};

/// Byte length of one SHA-1 entry inside the metainfo `pieces` field.
pub const PIECE_HASH_LEN: usize = 20;

/// Verifies assembled pieces against the torrent's per-piece SHA-1 hashes.
///
/// The expected digests come straight from the metainfo `pieces` field,
/// borrowed for `'a`. Piece shape — how long each piece is, and which
/// blocks tile it — stays the [`crate::downloader::BlockAssembler`]'s
/// contract: a piece of the wrong length simply hashes to the wrong digest
/// and verifies as `Ok(false)`.
///
/// A successful verdict marks the piece verified; a mismatch marks nothing
/// and leaves the piece retryable, mirroring how a session re-requests the
/// piece from another peer. Re-verifying an already-verified piece is a
/// typed error, exactly like re-filing an already-assembled one.
#[derive(Debug)]
pub struct PieceVerifier<'a> {
    piece_count: u32,
    expected: &'a [u8],
    verified: PieceField,
}

impl<'a> PieceVerifier<'a> {
    /// A verifier for `piece_count` pieces, judging against the
    /// concatenated 20-byte SHA-1 digests of the metainfo `pieces` field.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `piece_count` is zero or exceeds
    /// [`crate::downloader::MAX_PIECES`], `pieces` is not a multiple of
    /// [`PIECE_HASH_LEN`] bytes, or the digests do not number exactly
    /// `piece_count`.
    pub fn new(pieces: &'a [u8], piece_count: u32) -> Result<Self> {
        // Re-arms the shared 1..=MAX_PIECES guard on the verified field.
        let verified = PieceField::new(piece_count)?;
        if pieces.len() % PIECE_HASH_LEN != 0 {
            return Err(Error::Download(format!(
                "pieces is {} bytes, not a multiple of {PIECE_HASH_LEN}",
                pieces.len()
            )));
        }
        let carried = pieces.len() / PIECE_HASH_LEN;
        if carried != piece_count as usize {
            return Err(Error::Download(format!(
                "pieces carries {carried} digests, the torrent has {piece_count} pieces"
            )));
        }
        Ok(Self {
            piece_count,
            expected: pieces,
            verified,
        })
    }

    /// The piece count this verifier judges.
    #[must_use]
    pub const fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// The 20-byte digest the metainfo expects for piece `index`.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `index` is out of range.
    pub fn expected_hash(&self, index: u32) -> Result<&'a [u8]> {
        self.check_range(index)?;
        let start = index as usize * PIECE_HASH_LEN;
        Ok(&self.expected[start..start + PIECE_HASH_LEN])
    }

    /// Judges one assembled piece: `Ok(true)` when its SHA-1 matches the
    /// metainfo digest (and marks it verified), `Ok(false)` when the bytes
    /// hash differently (and mark nothing — the piece stays retryable).
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `index` is out of range or the
    /// piece was already verified — re-judging verified bytes is a caller
    /// bug worth surfacing, matching the assembler's stance.
    pub fn verify_piece(&mut self, index: u32, piece: &[u8]) -> Result<bool> {
        self.check_range(index)?;
        if self.verified.has(index) {
            return Err(Error::Download(format!(
                "piece {index} is already verified"
            )));
        }
        let digest: [u8; PIECE_HASH_LEN] = Sha1::digest(piece).into();
        let expected = self.expected_hash(index)?;
        if digest.as_slice() != expected {
            return Ok(false);
        }
        // In range by the check above: a typed error, never a panic.
        self.verified.set_have(index)?;
        Ok(true)
    }

    /// How many pieces have passed verification so far.
    #[must_use]
    pub fn verified_count(&self) -> u32 {
        self.verified.have_count()
    }

    /// `true` when every piece of the torrent has passed verification.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.verified.is_complete()
    }

    /// The verified-pieces field, ready for the session's have-list and the
    /// storage layer's completeness check.
    #[must_use]
    pub const fn verified_field(&self) -> &PieceField {
        &self.verified
    }

    /// Range check shared by judging and digest lookup.
    fn check_range(&self, index: u32) -> Result<()> {
        if index >= self.piece_count {
            return Err(Error::Download(format!(
                "piece {index} is beyond the {}-piece torrent",
                self.piece_count
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::BlockAssembler;

    /// Fills `length` bytes deterministically: offset `o` of piece `p` is
    /// `(7p + o) % 251`, the same pattern the downloader specs use.
    fn piece_bytes(piece: u32, length: u32) -> Vec<u8> {
        (0..length)
            .map(|offset| {
                u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251).unwrap_or(0)
            })
            .collect()
    }

    /// Hashes pieces with an independent SHA-1 pass — a decoder-bug-style
    /// mistake inside the verifier cannot mask itself.
    fn hash_pieces(pieces: &[Vec<u8>]) -> Vec<u8> {
        let mut hashes = Vec::new();
        for piece in pieces {
            let digest: [u8; PIECE_HASH_LEN] = Sha1::digest(piece).into();
            hashes.extend_from_slice(&digest);
        }
        hashes
    }

    #[test]
    fn constants_pin_the_verification_layout() {
        assert_eq!(PIECE_HASH_LEN, 20);
    }

    #[test]
    fn verifier_rejects_inconsistent_hash_slices() {
        // Zero pieces: the shared guard fires before anything else.
        let err = PieceVerifier::new(&[], 0).unwrap_err();
        assert!(err.to_string().contains("at least one piece"));
        // A hash slice that is not a multiple of 20 bytes.
        let err = PieceVerifier::new(&[0_u8; 23], 1).unwrap_err();
        assert!(err.to_string().contains("not a multiple of 20"));
        // A digest count that disagrees with the piece count.
        let err = PieceVerifier::new(&[0_u8; 40], 3).unwrap_err();
        assert!(
            err.to_string()
                .contains("carries 2 digests, the torrent has 3")
        );
        // Beyond the shared MAX_PIECES guard.
        let too_many = crate::downloader::MAX_PIECES + 1;
        assert!(PieceVerifier::new(&[], too_many).is_err());
    }

    #[test]
    fn expected_hash_addresses_the_raw_entries() {
        let pieces = vec![piece_bytes(0, 64), piece_bytes(1, 64), piece_bytes(2, 64)];
        let hashes = hash_pieces(&pieces);
        let verifier = PieceVerifier::new(&hashes, 3).unwrap();
        assert_eq!(verifier.piece_count(), 3);
        for index in 0..3_u32 {
            let start = index as usize * PIECE_HASH_LEN;
            assert_eq!(
                verifier.expected_hash(index).unwrap(),
                &hashes[start..start + PIECE_HASH_LEN]
            );
        }
        let err = verifier.expected_hash(3).unwrap_err();
        assert!(err.to_string().contains("beyond the 3-piece"));
    }

    #[test]
    fn verifier_accepts_correct_pieces_and_completes() {
        let pieces = vec![
            piece_bytes(0, 128),
            piece_bytes(1, 128),
            piece_bytes(2, 128),
        ];
        let hashes = hash_pieces(&pieces);
        let mut verifier = PieceVerifier::new(&hashes, 3).unwrap();
        assert_eq!(verifier.verified_count(), 0);
        assert!(!verifier.is_complete());
        for (index, piece) in pieces.iter().enumerate() {
            assert!(
                verifier
                    .verify_piece(u32::try_from(index).unwrap(), piece)
                    .unwrap()
            );
        }
        assert_eq!(verifier.verified_count(), 3);
        assert!(verifier.is_complete());
        assert_eq!(verifier.verified_field().have_count(), 3);
        for index in 0..3_u32 {
            assert!(verifier.verified_field().has(index));
        }
    }

    #[test]
    fn verifier_reports_mismatches_as_data_and_stays_retryable() {
        let pieces = vec![piece_bytes(0, 128), piece_bytes(1, 128)];
        let hashes = hash_pieces(&pieces);
        let mut verifier = PieceVerifier::new(&hashes, 2).unwrap();

        // One flipped byte: the verdict is Ok(false) and nothing is marked.
        let mut corrupt = piece_bytes(0, 128);
        corrupt[7] ^= 0x20;
        assert!(!verifier.verify_piece(0, &corrupt).unwrap());
        assert_eq!(verifier.verified_count(), 0);
        assert!(!verifier.is_complete());

        // The re-downloaded piece verifies: a mismatch is retryable data.
        assert!(verifier.verify_piece(0, &pieces[0]).unwrap());
        assert_eq!(verifier.verified_count(), 1);

        // A verified piece is closed for business, like an assembled one.
        let err = verifier.verify_piece(0, &pieces[0]).unwrap_err();
        assert!(err.to_string().contains("already verified"));

        // Out-of-range and empty pieces answer, never panic.
        assert!(verifier.verify_piece(2, &[]).is_err());
        assert!(!verifier.verify_piece(1, &[]).unwrap());
    }

    #[test]
    fn verifier_closes_the_assembler_loop() {
        // 2 pieces of 20_000 bytes: the classic out-of-order download.
        let pieces = vec![piece_bytes(0, 20_000), piece_bytes(1, 20_000)];
        let hashes = hash_pieces(&pieces);
        let mut assembler = BlockAssembler::new(2, 20_000, 40_000, 2).unwrap();
        let mut verifier = PieceVerifier::new(&hashes, 2).unwrap();

        let count = u32::try_from(pieces.len()).unwrap();
        for index in 0..count {
            // Last block of each piece first, mirroring a real swarm.
            let piece = &pieces[index as usize];
            let mut plans = assembler.block_plan(index).unwrap();
            plans.reverse();
            for plan in plans {
                let end = (plan.begin + plan.length) as usize;
                let block = &piece[plan.begin as usize..end];
                if let Some((done, bytes)) =
                    assembler.push_block(plan.index, plan.begin, block).unwrap()
                {
                    assert_eq!(done, index);
                    assert!(verifier.verify_piece(done, &bytes).unwrap());
                }
            }
        }
        assert_eq!(assembler.pieces_completed(), 2);
        assert!(verifier.is_complete());
        assert_eq!(verifier.verified_field().to_bitfield(), &[0b1100_0000]);
    }
}
