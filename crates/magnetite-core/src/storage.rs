//! The storage seam: where verified piece bytes live (M3 groundwork).
//!
//! Every shipped edge of the stack deliberately never held bytes: the
//! pump hands [`SessionEvent::PieceVerified`] bytes to the caller, the
//! upload policy names the exact block to serve, and the stream session
//! took a bare `lookup` closure at the I/O edge. `ARCHITECTURE.md` §4.8
//! pinned the placeholder: "the pump holds no bytes; storage stays M3."
//! This module is M3's groundwork — the [`Storage`] trait those edges
//! were waiting for, plus [`MemoryStorage`], the in-memory backend the
//! demos and tests share.
//!
//! The trait is deliberately small and runtime-free, like everything else
//! in the core:
//!
//! - [`Storage::write_piece`] takes one *verified* piece's full bytes —
//!   the shape `SessionEvent::PieceVerified` already hands over. A
//!   duplicate write is a typed [`Error::Download`]: the verifier never
//!   re-verifies a piece, so a duplicate means the same piece was handed
//!   over twice, which is an assembly bug, not normal flow.
//! - [`Storage::read_block`] answers the upload policy: `Ok(None)` for a
//!   piece that is not on hand yet is *data*, mirroring
//!   [`crate::session::ServeDecision::PieceNotVerified`], while a slice
//!   past the piece's end is a typed error (adversarial input, not flow).
//! - [`Storage::has_piece`] / [`Storage::held_count`] are the resume and
//!   completeness queries a long-lived client asks between sessions.
//!
//! Backends for real deployments — file-backed multi-file layouts, sparse
//! pre-allocated files, resume journals — implement the same four methods
//! and drop straight into the stream session's
//! [`crate::session_stream::StreamSession::step_with_storage`]; nothing
//! else in the stack changes.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// Where verified piece bytes live: the seam embedding apps back with
/// files, memory, or anything else.
pub trait Storage {
    /// Stores one *verified* piece's full bytes (the exact shape
    /// [`crate::session::SessionEvent::PieceVerified`] hands over).
    ///
    /// # Errors
    /// Implementations surface their own failures through the one typed
    /// taxonomy; a piece that is already held is
    /// [`Error::Download`] (the verifier never re-verifies, so a
    /// duplicate is an assembly bug).
    fn write_piece(&mut self, index: u32, piece: &[u8]) -> Result<()>;

    /// Slices one block out of a held piece, answering the upload policy's
    /// [`crate::session::ServeDecision::Serve`] plan. A piece that is not
    /// held is `Ok(None)` — data, not an error.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the slice runs past the piece's
    /// end (a layout violation, not a missing piece).
    fn read_block(&self, index: u32, begin: u32, length: u32) -> Result<Option<Vec<u8>>>;

    /// Whether the piece's verified bytes are held.
    fn has_piece(&self, index: u32) -> bool;

    /// How many pieces are held.
    fn held_count(&self) -> u32;
}

/// The in-memory [`Storage`] backend: verified pieces keyed by index, the
/// shape the demos previously ad-hoc'd with a `BTreeMap`.
///
/// Cloning shares nothing (the pieces are deep-copied), so a clone is a
/// cheap snapshot for forked flows.
#[derive(Debug, Clone, Default)]
pub struct MemoryStorage {
    pieces: BTreeMap<u32, Vec<u8>>,
}

impl MemoryStorage {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The stored bytes of one piece, if held — the raw view the tests and
    /// examples use to check what landed.
    #[must_use]
    pub fn piece(&self, index: u32) -> Option<&[u8]> {
        self.pieces.get(&index).map(Vec::as_slice)
    }
}

impl Storage for MemoryStorage {
    fn write_piece(&mut self, index: u32, piece: &[u8]) -> Result<()> {
        if self.pieces.contains_key(&index) {
            return Err(Error::Download(format!(
                "piece {index} is already held: verified pieces are written exactly once"
            )));
        }
        self.pieces.insert(index, piece.to_vec());
        Ok(())
    }

    fn read_block(&self, index: u32, begin: u32, length: u32) -> Result<Option<Vec<u8>>> {
        let Some(piece) = self.pieces.get(&index) else {
            return Ok(None);
        };
        let begin = usize::try_from(begin).unwrap_or(usize::MAX);
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        let end = match begin.checked_add(length) {
            Some(end) if end <= piece.len() => end,
            _ => {
                return Err(Error::Download(format!(
                    "block at {begin}..+{length} runs past piece {index}'s {} bytes",
                    piece.len()
                )));
            }
        };
        Ok(Some(piece[begin..end].to_vec()))
    }

    fn has_piece(&self, index: u32) -> bool {
        self.pieces.contains_key(&index)
    }

    fn held_count(&self) -> u32 {
        u32::try_from(self.pieces.len()).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_blocks_round_trip() {
        let mut storage = MemoryStorage::new();
        let piece: Vec<u8> = (0..48_u32)
            .map(|i| u8::try_from(i * 5).unwrap_or(0))
            .collect();
        storage.write_piece(3, &piece).expect("stored");
        let block = storage
            .read_block(3, 16, 16)
            .expect("in-range")
            .expect("held");
        assert_eq!(block, piece[16..32]);
        assert!(storage.has_piece(3));
        assert!(!storage.has_piece(0));
        assert_eq!(storage.held_count(), 1);
        assert_eq!(storage.piece(3), Some(piece.as_slice()));
    }

    #[test]
    fn duplicate_writes_are_typed_errors() {
        let mut storage = MemoryStorage::new();
        storage.write_piece(0, &[1, 2, 3]).expect("first write");
        let err = storage
            .write_piece(0, &[1, 2, 3])
            .expect_err("duplicate write");
        assert!(err.to_string().contains("already held"), "got {err}");
    }

    #[test]
    fn missing_pieces_read_as_none_and_out_of_range_slices_error() {
        let mut storage = MemoryStorage::new();
        storage.write_piece(1, &[7_u8; 100]).expect("stored");
        assert!(storage.read_block(9, 0, 4).expect("missing").is_none());
        let err = storage.read_block(1, 99, 2).expect_err("past the end");
        assert!(err.to_string().contains("runs past"), "got {err}");
        // A slice ending exactly at the piece's last byte is fine.
        assert_eq!(
            storage.read_block(1, 98, 2).expect("in-range"),
            Some(vec![7, 7])
        );
    }

    #[test]
    fn zero_length_blocks_read_empty() {
        let mut storage = MemoryStorage::new();
        storage.write_piece(0, &[1, 2]).expect("stored");
        assert_eq!(
            storage.read_block(0, 1, 0).expect("in-range"),
            Some(Vec::new())
        );
    }
}
