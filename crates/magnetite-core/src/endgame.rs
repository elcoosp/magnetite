//! Runtime-free swarm-level endgame bookkeeping: the duplicate-request
//! ledger the per-connection pump deferred.
//!
//! Block requests are per-connection state ([`crate::session::RequestPipeline`]),
//! so nothing stopped two connections from chasing the same block — wasteful
//! before the tail, and exactly what the protocol wants *in* the tail. The
//! endgame is the phase where every missing block already has a request in
//! flight somewhere in the swarm and the download would stall waiting on the
//! slowest peer: mainstream clients then deliberately duplicate the scarcest
//! in-flight blocks toward other connections, and the first delivery wins.
//!
//! [`EndgameBoard`] is the shared ledger that makes that policy honest: it
//! knows the torrent layout, which blocks are still missing, and how many
//! connections are chasing each one. The swarm's pumps consult it through
//! [`crate::session::SessionPump::next_request_endgame`] — fresh blocks are
//! handed out with no duplicates, the endgame is detected the moment none
//! are left, and the scarcest in-flight block is the one worth duplicating.
//! The caller feeds outcomes back: a delivered block through
//! [`EndgameBoard::mark_block`], a verified piece through
//! [`EndgameBoard::mark_piece`], a rejected piece reopened for retry through
//! [`EndgameBoard::reset_piece`].
//!
//! One board serves one torrent's *swarm*; every pump of that swarm shares
//! it. Nothing here knows a socket exists, no clock is read, and no
//! `Piece` bytes are held — the board is bookkeeping only.
//!
//! House rules, unchanged from the codec, tracker, peer wire, downloader,
//! and session edges:
//!
//! - Adversarial input is *data*: out-of-range pieces, misaligned or
//!   over-duplicated requests, and double deliveries are typed
//!   [`Error::Download`] values — never panics. Queries are total:
//!   out-of-range reads answer, they do not error.
//! - Allocation is bounded by the caller's piece layout, mirroring the
//!   downloader's [`crate::downloader::MAX_PIECES`] guard.
//! - The module is runtime-free and dependency-free.

use std::collections::{BTreeMap, BTreeSet};

use crate::downloader::{BlockPlan, PieceField};
use crate::error::{Error, Result};

/// How many connections may chase one block at once.
///
/// Endgame duplication only pays while the copies race *different* peers;
/// past a handful the bandwidth is pure waste. Three matches mainstream
/// practice (rqbit and libtorrent keep their endgame duplicates in the
/// same range) and bounds the ledger a hostile peer can inflate through
/// repeated requests.
pub const MAX_REQUESTS_PER_BLOCK: u32 = 3;

/// The swarm-level endgame ledger: which blocks are still missing, which
/// are delivered, and how many connections chase each one.
///
/// The board mirrors the torrent layout the downloader already enforces
/// (16 KiB block tiling, one short final piece), so its plans are
/// interchangeable with [`crate::downloader::BlockAssembler::block_plan`] —
/// property specs cross-check the two tilings against each other.
#[derive(Debug, Clone)]
pub struct EndgameBoard {
    piece_count: u32,
    piece_length: u32,
    last_piece_length: u32,
    total_length: u64,
    /// Pieces that passed verification: their blocks are served truth,
    /// never requested again.
    done: PieceField,
    /// Blocks that arrived, per piece still short of verification.
    delivered: BTreeMap<u32, BTreeSet<u32>>,
    /// In-flight request counts, per piece and block offset.
    requests: BTreeMap<u32, BTreeMap<u32, u32>>,
}

impl EndgameBoard {
    /// A ledger for a torrent of `piece_count` pieces (every piece but the
    /// last `piece_length` bytes long, ending at `total_length`).
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the layout is inconsistent — the
    /// same guards [`crate::downloader::BlockAssembler::new`] enforces,
    /// word for word, so one hostile layout cannot split the two halves
    /// of the downloader.
    pub fn new(piece_count: u32, piece_length: u32, total_length: u64) -> Result<Self> {
        // Re-arms the shared 1..=MAX_PIECES guard on the done field.
        let done = PieceField::new(piece_count)?;
        if piece_length == 0 {
            return Err(Error::Download(
                "piece length must be at least one byte".into(),
            ));
        }
        let preceding = u64::from(piece_count - 1) * u64::from(piece_length);
        let last_piece_length =
            u32::try_from(total_length.saturating_sub(preceding)).map_err(|_| {
                Error::Download(format!(
                    "total length {total_length} leaves a last piece too long for u32 bytes"
                ))
            })?;
        if last_piece_length == 0 || last_piece_length > piece_length {
            return Err(Error::Download(format!(
                "total length {total_length} leaves a last piece of {last_piece_length} bytes, \
                 outside 1..={piece_length}"
            )));
        }
        Ok(Self {
            piece_count,
            piece_length,
            last_piece_length,
            total_length,
            done,
            delivered: BTreeMap::new(),
            requests: BTreeMap::new(),
        })
    }

    /// The piece count this ledger is laid out for.
    #[must_use]
    pub const fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// The layout's piece length.
    #[must_use]
    pub const fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// The layout's total payload length.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// How many blocks are still missing: not delivered, and not part of a
    /// verified piece.
    #[must_use]
    pub fn missing_blocks(&self) -> u32 {
        let mut missing = 0;
        for index in 0..self.piece_count {
            if self.done.has(index) {
                continue;
            }
            let count = self.block_count(index);
            let received =
                u32::try_from(self.delivered.get(&index).map_or(0, BTreeSet::len)).unwrap_or(0);
            missing += count.saturating_sub(received);
        }
        missing
    }

    /// Whether piece `index` has passed verification. Total: out-of-range
    /// indexes answer `false`, never panic — the guard the caller checks
    /// before [`EndgameBoard::mark_piece`] so a racing duplicate delivery
    /// (two connections chased the same block; both assembled it) stays
    /// data, not an error.
    #[must_use]
    pub fn is_done(&self, index: u32) -> bool {
        self.done.has(index)
    }

    /// Whether the exact block `(index, begin)` is still missing. Total:
    /// out-of-range or non-layout blocks answer `false`, never panic.
    #[must_use]
    pub fn is_missing(&self, index: u32, begin: u32) -> bool {
        if self.done.has(index) {
            return false;
        }
        match self.block_length(index, begin) {
            Some(_) => !self
                .delivered
                .get(&index)
                .is_some_and(|blocks| blocks.contains(&begin)),
            None => false,
        }
    }

    /// How many connections currently chase the exact block `(index,
    /// begin)`. Total: anything unknown answers `0`, never panics.
    #[must_use]
    pub fn requests_for(&self, index: u32, begin: u32) -> u32 {
        self.requests
            .get(&index)
            .and_then(|blocks| blocks.get(&begin))
            .copied()
            .unwrap_or(0)
    }

    /// Whether the endgame is on: at least one block is missing and every
    /// missing block already has a request in flight somewhere.
    #[must_use]
    pub fn in_endgame(&self) -> bool {
        if self.missing_blocks() == 0 {
            return false;
        }
        for index in 0..self.piece_count {
            if self.done.has(index) {
                continue;
            }
            let received = self.delivered.get(&index);
            for begin in self.block_begins(index) {
                if received.is_some_and(|blocks| blocks.contains(&begin)) {
                    continue;
                }
                if self.requests_for(index, begin) == 0 {
                    return false;
                }
            }
        }
        true
    }

    /// The missing block worth duplicating in the endgame: the scarcest
    /// in-flight demand first, ties to the lowest piece index and offset —
    /// spreading the chase evenly, exactly like the downloader's
    /// rarest-first ranking. `None` while the endgame is off (a fresh
    /// block exists somewhere, or nothing is missing).
    #[must_use]
    pub fn pick_endgame(&self) -> Option<BlockPlan> {
        if !self.in_endgame() {
            return None;
        }
        let mut best: Option<(u32, u32, u32)> = None;
        for index in 0..self.piece_count {
            if self.done.has(index) {
                continue;
            }
            for begin in self.block_begins(index) {
                if !self.is_missing(index, begin) {
                    continue;
                }
                let count = self.requests_for(index, begin);
                if count == 0 {
                    continue;
                }
                if best.is_none_or(|(best_count, best_index, best_begin)| {
                    (count, index, begin) < (best_count, best_index, best_begin)
                }) {
                    best = Some((count, index, begin));
                }
            }
        }
        best.map(|(_, index, begin)| BlockPlan {
            index,
            begin,
            length: self.block_length(index, begin).unwrap_or_default(),
        })
    }

    /// Records one outbound request for `plan`: the block gains a chaser.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the plan does not match the layout
    /// (out-of-range piece, misaligned or out-of-range begin, wrong block
    /// length), the block was already delivered, its piece already
    /// verified, or it is already chased by [`MAX_REQUESTS_PER_BLOCK`]
    /// connections.
    pub fn record_request(&mut self, plan: &BlockPlan) -> Result<()> {
        self.layout_check(plan)?;
        if self.done.has(plan.index) {
            return Err(Error::Download(format!(
                "piece {} is already verified",
                plan.index
            )));
        }
        if self
            .delivered
            .get(&plan.index)
            .is_some_and(|blocks| blocks.contains(&plan.begin))
        {
            return Err(Error::Download(format!(
                "piece {} block at {} was already received",
                plan.index, plan.begin
            )));
        }
        let chased = self.requests_for(plan.index, plan.begin);
        if chased >= MAX_REQUESTS_PER_BLOCK {
            return Err(Error::Download(format!(
                "piece {} block at {} already has {chased} in-flight requests \
                 (cap {MAX_REQUESTS_PER_BLOCK})",
                plan.index, plan.begin
            )));
        }
        self.requests
            .entry(plan.index)
            .or_default()
            .insert(plan.begin, chased + 1);
        Ok(())
    }

    /// Retires one in-flight request — its caller canceled it or its peer
    /// choked. Returns whether a request was actually counted.
    pub fn release_request(&mut self, index: u32, begin: u32) -> bool {
        let Some(blocks) = self.requests.get_mut(&index) else {
            return false;
        };
        match blocks.get_mut(&begin) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                blocks.remove(&begin);
                if blocks.is_empty() {
                    self.requests.remove(&index);
                }
                true
            }
            None => false,
        }
    }

    /// Marks the exact block `(index, begin)` delivered: it leaves the
    /// missing set and its outstanding demand is dropped — a delivered
    /// block needs no more chasers, and any connection still holding a
    /// request for it is free to cancel.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the block does not match the
    /// layout, its piece is already verified, or the block was already
    /// delivered — double deliveries are caller bugs worth surfacing,
    /// matching the assembler's stance on duplicates.
    pub fn mark_block(&mut self, index: u32, begin: u32) -> Result<()> {
        if self.done.has(index) {
            return Err(Error::Download(format!(
                "piece {index} is already verified"
            )));
        }
        if begin % crate::downloader::BLOCK_LEN != 0 {
            return Err(Error::Download(format!(
                "block begin {begin} is not {}-byte aligned",
                crate::downloader::BLOCK_LEN
            )));
        }
        if self
            .delivered
            .get(&index)
            .is_some_and(|blocks| blocks.contains(&begin))
        {
            return Err(Error::Download(format!(
                "piece {index} block at {begin} was already received"
            )));
        }
        self.block_length(index, begin).ok_or_else(|| {
            Error::Download(format!(
                "piece {index} is beyond the {}-piece torrent",
                self.piece_count
            ))
        })?;
        self.delivered.entry(index).or_default().insert(begin);
        if let Some(blocks) = self.requests.get_mut(&index) {
            blocks.remove(&begin);
            if blocks.is_empty() {
                self.requests.remove(&index);
            }
        }
        Ok(())
    }

    /// Marks piece `index` verified: its blocks (delivered or chased)
    /// leave the ledger for good.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the index is out of range or the
    /// piece was already marked — mirroring the verifier's stance on
    /// re-judged bytes.
    pub fn mark_piece(&mut self, index: u32) -> Result<()> {
        if index >= self.piece_count {
            return Err(Error::Download(format!(
                "piece {index} is beyond the {}-piece torrent",
                self.piece_count
            )));
        }
        if self.done.has(index) {
            return Err(Error::Download(format!(
                "piece {index} is already verified"
            )));
        }
        self.done.set_have(index)?;
        self.delivered.remove(&index);
        self.requests.remove(&index);
        Ok(())
    }

    /// Reopens piece `index` for a clean retry after a rejection: its
    /// delivered blocks and outstanding demand are cleared, so every block
    /// of the piece is missing and unchased again — the swarm-level half of
    /// the pump's re-request loop.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the index is out of range or the
    /// piece is verified — a requeue applies only to rejected pieces.
    pub fn reset_piece(&mut self, index: u32) -> Result<()> {
        if index >= self.piece_count {
            return Err(Error::Download(format!(
                "piece {index} is beyond the {}-piece torrent",
                self.piece_count
            )));
        }
        if self.done.has(index) {
            return Err(Error::Download(format!(
                "piece {index} is verified; a requeue applies only to rejected pieces"
            )));
        }
        self.delivered.remove(&index);
        self.requests.remove(&index);
        Ok(())
    }

    /// The block length the layout plans at `(index, begin)`, or `None`
    /// when the pair is not a block start of the torrent.
    fn block_length(&self, index: u32, begin: u32) -> Option<u32> {
        if index >= self.piece_count || begin % crate::downloader::BLOCK_LEN != 0 {
            return None;
        }
        let length = self.piece_len(index);
        if begin >= length {
            return None;
        }
        Some(self.block_len(index, begin))
    }

    /// Every block start of piece `index`, in ascending order.
    fn block_begins(&self, index: u32) -> impl Iterator<Item = u32> {
        let step = usize::try_from(crate::downloader::BLOCK_LEN).unwrap_or(1);
        let length = self.piece_len(index);
        (0..length).step_by(step)
    }

    /// The byte length of piece `index` (the last piece may be shorter).
    fn piece_len(&self, index: u32) -> u32 {
        if index + 1 == self.piece_count {
            self.last_len()
        } else {
            self.piece_length
        }
    }

    /// The planned length of the block starting at `begin` in piece
    /// `index`: [`crate::downloader::BLOCK_LEN`] except at the end of a
    /// piece, where the remainder is shorter.
    fn block_len(&self, index: u32, begin: u32) -> u32 {
        let length = self.piece_len(index);
        match begin.checked_add(crate::downloader::BLOCK_LEN) {
            // Overflow-proof: a wrapped or past-the-end advance means the
            // remainder block is the shorter one, mirroring the
            // assembler's checked advance in `block_plan`.
            Some(next) if next <= length => crate::downloader::BLOCK_LEN,
            _ => length - begin,
        }
    }

    /// How many blocks tile piece `index`.
    fn block_count(&self, index: u32) -> u32 {
        self.piece_len(index).div_ceil(crate::downloader::BLOCK_LEN)
    }

    /// Layout validation shared by the record and delivery seams: answers
    /// the planned block length, or a typed error naming the violation.
    fn layout_check(&self, plan: &BlockPlan) -> Result<u32> {
        if plan.index >= self.piece_count {
            return Err(Error::Download(format!(
                "piece {} is beyond the {}-piece torrent",
                plan.index, self.piece_count
            )));
        }
        let length = self.piece_len(plan.index);
        if plan.begin >= length {
            return Err(Error::Download(format!(
                "block begin {} lies beyond piece {}'s {length} bytes",
                plan.begin, plan.index
            )));
        }
        if plan.begin % crate::downloader::BLOCK_LEN != 0 {
            return Err(Error::Download(format!(
                "block begin {} is not {}-byte aligned",
                plan.begin,
                crate::downloader::BLOCK_LEN
            )));
        }
        let planned = self.block_len(plan.index, plan.begin);
        if plan.length != planned {
            return Err(Error::Download(format!(
                "piece {} block at {} is {} bytes, the layout plans {planned}",
                plan.index, plan.begin, plan.length
            )));
        }
        Ok(planned)
    }

    /// The final piece's byte length, fixed at construction.
    fn last_len(&self) -> u32 {
        self.last_piece_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::BLOCK_LEN;

    /// The canonical layout used across these specs: two 20_000-byte
    /// pieces, each tiling into one full [`BLOCK_LEN`] block and one
    /// 3_616-byte remainder.
    fn board() -> EndgameBoard {
        EndgameBoard::new(2, 20_000, 40_000).unwrap()
    }

    fn plan(index: u32, begin: u32, length: u32) -> BlockPlan {
        BlockPlan {
            index,
            begin,
            length,
        }
    }

    #[test]
    fn constants_pin_the_endgame_layout() {
        assert_eq!(MAX_REQUESTS_PER_BLOCK, 3);
    }

    #[test]
    fn board_rejects_inconsistent_layouts() {
        // Zero pieces: the shared guard fires before anything else.
        let error = EndgameBoard::new(0, 20_000, 40_000).unwrap_err();
        assert!(error.to_string().contains("at least one piece"));
        // Beyond the shared MAX_PIECES guard.
        let too_many = crate::downloader::MAX_PIECES + 1;
        assert!(EndgameBoard::new(too_many, 20_000, 40_000).is_err());
        // A zero piece length.
        let error = EndgameBoard::new(2, 0, 40_000).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("piece length must be at least one byte")
        );
        // A total that leaves an empty last piece.
        let error = EndgameBoard::new(2, 20_000, 0).unwrap_err();
        assert!(error.to_string().contains("outside 1..=20000"));
    }

    #[test]
    fn board_starts_fresh_and_counts_missing() {
        let board = board();
        assert_eq!(board.piece_count(), 2);
        assert_eq!(board.piece_length(), 20_000);
        assert_eq!(board.total_length(), 40_000);
        assert_eq!(board.missing_blocks(), 4);
        assert!(!board.in_endgame());
        assert_eq!(board.pick_endgame(), None);
        // Queries are total: out-of-range reads answer, never panic. The
        // second block start of piece 0 is missing; a begin past the
        // piece's 20_000 bytes is not a block at all.
        assert!(!board.is_missing(2, 0));
        assert!(board.is_missing(0, BLOCK_LEN));
        assert!(!board.is_missing(0, 20_000));
        assert_eq!(board.requests_for(0, 0), 0);
        assert_eq!(board.requests_for(9, 9), 0);
    }

    #[test]
    fn board_tracks_requests_and_enforces_the_cap() {
        let mut board = board();
        let block = plan(0, 0, BLOCK_LEN);
        board.record_request(&block).unwrap();
        assert_eq!(board.requests_for(0, 0), 1);
        board.record_request(&block).unwrap();
        board.record_request(&block).unwrap();
        assert_eq!(board.requests_for(0, 0), MAX_REQUESTS_PER_BLOCK);
        let error = board.record_request(&block).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already has 3 in-flight requests")
        );
        // A release frees a chase slot.
        assert!(board.release_request(0, 0));
        assert_eq!(board.requests_for(0, 0), 2);
        board.record_request(&block).unwrap();
        assert_eq!(board.requests_for(0, 0), 3);
        // Draining to zero removes the entry entirely.
        for _ in 0..3 {
            assert!(board.release_request(0, 0));
        }
        assert_eq!(board.requests_for(0, 0), 0);
        assert!(!board.release_request(0, 0));
        assert!(!board.release_request(9, 0));
        // In-flight demand is invisible to the missing count.
        assert_eq!(board.missing_blocks(), 4);
        assert!(!board.in_endgame());
    }

    #[test]
    fn board_record_rejects_hostile_plans() {
        let mut board = board();
        // Out-of-range piece.
        let error = board.record_request(&plan(2, 0, BLOCK_LEN)).unwrap_err();
        assert!(error.to_string().contains("beyond"));
        // Misaligned begin.
        let error = board.record_request(&plan(0, 1, BLOCK_LEN)).unwrap_err();
        assert!(error.to_string().contains("not 16384-byte aligned"));
        // Begin beyond the piece.
        let error = board
            .record_request(&plan(0, 20_000, BLOCK_LEN))
            .unwrap_err();
        assert!(error.to_string().contains("lies beyond piece 0"));
        // Wrong block length.
        let error = board.record_request(&plan(0, 0, 100)).unwrap_err();
        assert!(error.to_string().contains("the layout plans 16384"));
        // The short final block of the last piece is layout-exact.
        board.record_request(&plan(1, BLOCK_LEN, 3_616)).unwrap();
        assert_eq!(board.requests_for(1, BLOCK_LEN), 1);
    }

    #[test]
    fn board_mark_block_drops_demand_and_missing() {
        let mut board = board();
        let block = plan(0, 0, BLOCK_LEN);
        board.record_request(&block).unwrap();
        board.record_request(&block).unwrap();
        board.mark_block(0, 0).unwrap();
        // Delivered: no longer missing, no longer chased.
        assert!(!board.is_missing(0, 0));
        assert_eq!(board.missing_blocks(), 3);
        assert_eq!(board.requests_for(0, 0), 0);
        // A double delivery is a caller bug, matching the assembler.
        let error = board.mark_block(0, 0).unwrap_err();
        assert!(error.to_string().contains("already received"));
        // Requests against a delivered block are refused, not counted.
        let error = board.record_request(&block).unwrap_err();
        assert!(error.to_string().contains("already received"));
        // Hostile deliveries are typed errors too.
        assert!(board.mark_block(2, 0).is_err());
        assert!(board.mark_block(0, 1).is_err());
        assert!(board.mark_block(1, BLOCK_LEN).is_ok());
    }

    #[test]
    fn board_mark_piece_and_reset_piece_flip_whole_pieces() {
        let mut board = board();
        board.record_request(&plan(0, 0, BLOCK_LEN)).unwrap();
        board.mark_block(0, 0).unwrap();
        board.mark_piece(0).unwrap();
        // The whole piece left the ledger.
        assert_eq!(board.missing_blocks(), 2);
        assert!(!board.is_missing(0, BLOCK_LEN));
        assert!(!board.is_missing(0, 0));
        assert_eq!(board.requests_for(0, BLOCK_LEN), 0);
        // The done query mirrors the verifier's field, out-of-range safe.
        assert!(board.is_done(0));
        assert!(!board.is_done(1));
        assert!(!board.is_done(2));
        // A second mark is a caller bug, like re-judging verified bytes.
        let error = board.mark_piece(0).unwrap_err();
        assert!(error.to_string().contains("already verified"));
        // Hostile marks are typed errors.
        let error = board.mark_piece(2).unwrap_err();
        assert!(error.to_string().contains("beyond"));
        // Reset reopens a piece: delivered and chased state cleared.
        board.record_request(&plan(1, 0, BLOCK_LEN)).unwrap();
        board.mark_block(1, 0).unwrap();
        board.reset_piece(1).unwrap();
        assert_eq!(board.missing_blocks(), 2);
        assert!(board.is_missing(1, 0));
        assert!(board.is_missing(1, BLOCK_LEN));
        assert_eq!(board.requests_for(1, 0), 0);
        // A verified piece has nothing to reopen.
        board.mark_piece(1).unwrap();
        let error = board.reset_piece(1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requeue applies only to rejected pieces")
        );
        // Out-of-range resets are typed errors.
        assert!(board.reset_piece(2).is_err());
    }

    #[test]
    fn endgame_requires_every_missing_block_chased() {
        let mut board = board();
        // Chase three of the four blocks.
        board.record_request(&plan(0, 0, BLOCK_LEN)).unwrap();
        board.record_request(&plan(0, BLOCK_LEN, 3_616)).unwrap();
        board.record_request(&plan(1, 0, BLOCK_LEN)).unwrap();
        assert!(!board.in_endgame());
        // The last fresh block enters flight: the endgame is on.
        board.record_request(&plan(1, BLOCK_LEN, 3_616)).unwrap();
        assert!(board.in_endgame());
        // Delivering one block shrinks the tail; every remaining block is
        // still chased, so the endgame stays on.
        board.mark_block(0, 0).unwrap();
        assert!(board.in_endgame());
        assert_eq!(board.missing_blocks(), 3);
        // A full release drains one chase: fresh again, endgame off.
        assert!(board.release_request(1, BLOCK_LEN));
        assert!(!board.in_endgame());
        // A verified piece shrinks the tail: the rest chased, endgame on.
        board.record_request(&plan(1, BLOCK_LEN, 3_616)).unwrap();
        board.mark_piece(0).unwrap();
        assert!(board.in_endgame());
        assert_eq!(board.missing_blocks(), 2);
    }

    #[test]
    fn pick_endgame_prefers_the_scarcest_then_the_lowest() {
        let mut board = board();
        // (0,0) and (1,1) chased once, (1,0) twice; (0,1) not chased, so
        // the endgame is off and the pick refuses.
        board.record_request(&plan(0, 0, BLOCK_LEN)).unwrap();
        board.record_request(&plan(1, 0, BLOCK_LEN)).unwrap();
        board.record_request(&plan(1, 0, BLOCK_LEN)).unwrap();
        board.record_request(&plan(1, BLOCK_LEN, 3_616)).unwrap();
        assert!(!board.in_endgame());
        assert_eq!(board.pick_endgame(), None);
        // (0,1) enters flight: endgame on, scarcest is a count-1 tie
        // between (0,0) and (1,1) — the lowest piece wins.
        board.record_request(&plan(0, BLOCK_LEN, 3_616)).unwrap();
        assert!(board.in_endgame());
        assert_eq!(board.pick_endgame(), Some(plan(0, 0, BLOCK_LEN)));
        // (0,0) delivered: the tail shrinks, every remaining block is
        // still chased, and the count-1 tie moves to (0,1).
        board.mark_block(0, 0).unwrap();
        assert!(board.in_endgame());
        assert_eq!(board.pick_endgame(), Some(plan(0, BLOCK_LEN, 3_616)));
        // (0,1) delivered too: only the double-chased (1,0) and the
        // single-chased (1,1) remain — the scarcest wins.
        board.mark_block(0, BLOCK_LEN).unwrap();
        assert!(board.in_endgame());
        assert_eq!(board.pick_endgame(), Some(plan(1, BLOCK_LEN, 3_616)));
        // (1,1) delivered as well: only the double-chased (1,0) remains.
        board.mark_block(1, BLOCK_LEN).unwrap();
        assert!(board.in_endgame());
        assert_eq!(board.pick_endgame(), Some(plan(1, 0, BLOCK_LEN)));
    }
}
