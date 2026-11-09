//! Runtime-free piece downloader bookkeeping: piece availability (BEP 3
//! bitfields), rarest-first piece selection, and bounded block assembly.
//!
//! This is the heart the piece downloader needs between the wire codec and
//! the socket: [`peer_wire`](crate::peer_wire) turns bytes into
//! [`Message`] values, and this module turns
//! those messages into download state — which pieces exist, which ones a
//! peer has, which piece to request next, and when the blocks of a piece
//! tile together into the bytes a caller SHA-1-verifies. There is no I/O
//! here at all, so the future `tokio` session is a thin pump that routes
//! incoming [`Message::Piece`] payloads into
//! [`BlockAssembler::push_message`] and outgoing requests from
//! [`BlockPlan::to_message`].
//!
//! House rules, unchanged from the tracker and peer wire edges:
//!
//! - Adversarial input is *data*: out-of-range pieces, misaligned or
//!   duplicated blocks, and forged layouts are typed
//!   [`Error::Download`] values — never panics.
//! - Allocation is bounded by explicit guards ([`MAX_PIECES`],
//!   [`MAX_BUFFERED_PIECES`]) and by the caller's piece layout, mirroring
//!   the codec's `MAX_DEPTH` and the framing cap.
//! - The module is runtime-free and dependency-free; nothing here knows a
//!   socket exists.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::peer_wire::Message;

/// Standard block size for `request` messages, 16 KiB (BEP 3 practice).
///
/// Every planned block is exactly this long except the final block of a
/// piece, which is as long as the remaining piece bytes.
pub const BLOCK_LEN: u32 = 16_384;

/// Upper bound for the piece count the bookkeeping accepts, 2²⁴ pieces.
///
/// Real torrents stay orders of magnitude below this; the cap mirrors
/// `MAX_DEPTH` and `MAX_MESSAGE_LEN`: a forged metainfo with billions of
/// pieces is a typed error instead of an unbounded allocation (the cap
/// bounds a bitfield at 2 MiB).
pub const MAX_PIECES: u32 = 1 << 24;

/// Upper bound for partial pieces buffered at once, 256.
///
/// The in-flight window a well-behaved session keeps open is far smaller;
/// the cap exists so a caller bug cannot accumulate one buffer per piece
/// of a giant torrent before anyone notices.
pub const MAX_BUFFERED_PIECES: u32 = 256;

/// Which pieces of a torrent we (or a peer) already have: a bitset in the
/// BEP 3 wire order (first byte is pieces 0–7, most-significant bit first).
///
/// Parsing is strict about *shape* — a bit that names a piece beyond the
/// torrent's count is a typed error, matching how [`Message::Bitfield`]
/// payloads are validated — and queries are total: [`PieceField::has`]
/// answers `false` for any out-of-range index instead of panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceField {
    piece_count: u32,
    bytes: Vec<u8>,
}

impl PieceField {
    /// An empty field over `piece_count` pieces: nothing have yet.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `piece_count` is zero or exceeds
    /// [`MAX_PIECES`].
    pub fn new(piece_count: u32) -> Result<Self> {
        validate_piece_count(piece_count)?;
        let len = wire_len(piece_count);
        Ok(Self {
            piece_count,
            bytes: vec![0_u8; len],
        })
    }

    /// Parses a peer's `Bitfield` message payload (BEP 3 bit order).
    ///
    /// A shorter-than-canonical bitfield is accepted — trailing all-zero
    /// bytes are commonly elided — and an empty payload is a legal
    /// "I have nothing". Bits beyond the piece count are rejected, not
    /// masked: a peer that sets padding bits is speaking the wrong shape.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `piece_count` is zero or exceeds
    /// [`MAX_PIECES`], the payload is longer than the wire form, or a bit
    /// beyond the piece count is set.
    pub fn from_bitfield(wire: &[u8], piece_count: u32) -> Result<Self> {
        validate_piece_count(piece_count)?;
        let canonical = wire_len(piece_count);
        if wire.len() > canonical {
            return Err(Error::Download(format!(
                "bitfield is {} bytes, the {piece_count}-piece torrent allows {canonical}",
                wire.len()
            )));
        }
        let mut first_piece = 0_u32;
        for &byte in wire {
            if byte != 0 {
                for bit in 0..8_u32 {
                    let index = first_piece + bit;
                    if index >= piece_count && byte & (1 << (7 - bit)) != 0 {
                        return Err(Error::Download(format!(
                            "bitfield marks piece {index}, beyond the {piece_count}-piece torrent"
                        )));
                    }
                }
            }
            first_piece += 8;
        }
        let mut bytes = vec![0_u8; canonical];
        bytes[..wire.len()].copy_from_slice(wire);
        Ok(Self { piece_count, bytes })
    }

    /// The canonical wire form: exactly `ceil(piece_count / 8)` bytes with
    /// zero padding, ready for a [`Message::Bitfield`].
    #[must_use]
    pub fn to_bitfield(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// The piece count this field is sized for.
    #[must_use]
    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// How many pieces are marked have.
    #[must_use]
    pub fn have_count(&self) -> u32 {
        self.bytes.iter().map(|byte| byte.count_ones()).sum()
    }

    /// How many pieces are still missing.
    #[must_use]
    pub fn missing_count(&self) -> u32 {
        self.piece_count - self.have_count()
    }

    /// `true` when every piece of the torrent is marked have.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.have_count() == self.piece_count
    }

    /// Whether piece `index` is marked have; out-of-range indexes are
    /// simply not had (queries never panic).
    #[must_use]
    pub fn has(&self, index: u32) -> bool {
        if index >= self.piece_count {
            return false;
        }
        self.byte_at(index) & bit_mask(index) != 0
    }

    /// Marks piece `index` have.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `index` is out of range.
    pub fn set_have(&mut self, index: u32) -> Result<()> {
        self.check_range(index)?;
        self.bytes[(index / 8) as usize] |= bit_mask(index);
        Ok(())
    }

    /// The lowest piece index still missing, if any.
    #[must_use]
    pub fn first_missing(&self) -> Option<u32> {
        (0..self.piece_count).find(|&index| !self.has(index))
    }

    /// Iterates the piece indexes marked have, ascending.
    pub fn iter_have(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.piece_count).filter(|&index| self.has(index))
    }

    /// Iterates the piece indexes still missing, ascending.
    pub fn iter_missing(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.piece_count).filter(|&index| !self.has(index))
    }

    fn byte_at(&self, index: u32) -> u8 {
        self.bytes[(index / 8) as usize]
    }

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

/// Per-piece count of how many connected peers have each piece — the
/// input rarest-first selection needs.
///
/// Counts saturate instead of overflowing, and every method treats
/// out-of-range indexes as "no peer has that" or a typed error, never a
/// panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    piece_count: u32,
    counts: Vec<u32>,
}

impl Availability {
    /// Zeroed counts over `piece_count` pieces.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `piece_count` is zero or exceeds
    /// [`MAX_PIECES`].
    pub fn new(piece_count: u32) -> Result<Self> {
        validate_piece_count(piece_count)?;
        Ok(Self {
            piece_count,
            counts: vec![0_u32; piece_count as usize],
        })
    }

    /// The piece count these counts are sized for.
    #[must_use]
    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// Registers a peer whose bitfield just arrived: +1 per have bit.
    pub fn add_peer(&mut self, field: &PieceField) {
        for index in field.iter_have() {
            self.bump(index, 1);
        }
    }

    /// Deregisters a disconnecting peer: -1 per have bit, saturating at
    /// zero so bookkeeping bugs stay visible instead of wrapping.
    pub fn remove_peer(&mut self, field: &PieceField) {
        for index in field.iter_have() {
            self.bump(index, -1);
        }
    }

    /// Applies a peer's [`Message::Have`] for piece `index`: +1.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `index` is out of range — a peer
    /// announcing a piece the torrent does not have is a protocol
    /// violation, not something to mask.
    pub fn update(&mut self, index: u32) -> Result<()> {
        if index >= self.piece_count {
            return Err(Error::Download(format!(
                "have for piece {index}, beyond the {}-piece torrent",
                self.piece_count
            )));
        }
        self.bump(index, 1);
        Ok(())
    }

    /// How many peers have piece `index`; out-of-range indexes read 0.
    #[must_use]
    pub fn count(&self, index: u32) -> u32 {
        if index >= self.piece_count {
            return 0;
        }
        self.counts[index as usize]
    }

    fn bump(&mut self, index: u32, direction: i8) {
        let slot = &mut self.counts[index as usize];
        if direction > 0 {
            *slot = slot.saturating_add(1);
        } else {
            *slot = slot.saturating_sub(1);
        }
    }
}

/// How the next piece to download is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PieceStrategy {
    /// Lowest-indexed missing piece: deterministic and baseline-able.
    #[default]
    Sequential,
    /// Scarcest piece first (fewest peers have it), ties broken by the
    /// lowest index — the classic swarm-throughput heuristic. Without an
    /// [`Availability`] this degrades to [`PieceStrategy::Sequential`].
    RarestFirst,
}

/// Chooses the next piece to download from one peer's availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PiecePicker {
    strategy: PieceStrategy,
}

impl PiecePicker {
    /// A picker for `strategy`.
    #[must_use]
    pub const fn new(strategy: PieceStrategy) -> Self {
        Self { strategy }
    }

    /// The strategy this picker applies.
    #[must_use]
    pub const fn strategy(self) -> PieceStrategy {
        self.strategy
    }

    /// Picks the next piece index among the pieces `theirs` has and `ours`
    /// misses, or `None` when the peer offers nothing we need.
    ///
    /// Rarest-first ranks candidates by their [`Availability`] count
    /// (ties to the lowest index); sequential ranks by index. Both are
    /// pure reads, so callers may pick freely before every request.
    #[must_use]
    pub fn pick(
        &self,
        ours: &PieceField,
        theirs: &PieceField,
        availability: Option<&Availability>,
    ) -> Option<u32> {
        let mut best = None;
        let mut best_rank = (u32::MAX, u32::MAX);
        for index in theirs.iter_have() {
            if ours.has(index) {
                continue;
            }
            let rank = match self.strategy {
                PieceStrategy::Sequential => (0_u32, index),
                PieceStrategy::RarestFirst => {
                    (availability.map_or(0, |counts| counts.count(index)), index)
                }
            };
            if rank < best_rank {
                best_rank = rank;
                best = Some(index);
            }
        }
        best
    }
}

/// One 16 KiB-or-less `request` block of a piece: exactly the three wire
/// fields of [`Message::Request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPlan {
    /// Piece index.
    pub index: u32,
    /// Byte offset inside the piece, [`BLOCK_LEN`]-aligned.
    pub begin: u32,
    /// Block length: [`BLOCK_LEN`] except at a piece's end.
    pub length: u32,
}

impl BlockPlan {
    /// The [`Message::Request`] this plan encodes as.
    #[must_use]
    pub fn to_message(self) -> Message<'static> {
        Message::Request {
            index: self.index,
            begin: self.begin,
            length: self.length,
        }
    }
}

/// The received blocks of one piece still being assembled.
#[derive(Debug)]
struct PartialPiece {
    blocks: BTreeMap<u32, Vec<u8>>,
    expected: u32,
}

/// Tiles pieces into [`BLOCK_LEN`] blocks and reassembles incoming blocks
/// into whole pieces, bounded and strict.
///
/// This is the swarm-wide truth of "which bytes do we hold": every peer's
/// [`Message::Piece`] payloads go through [`BlockAssembler::push_block`]
/// (or [`BlockAssembler::push_message`]), and each piece's bytes come back
/// exactly once, ready for the caller's SHA-1 check against the metainfo.
/// Request bookkeeping stays on the per-connection side; the assembler
/// accepts any block of any piece, out of order, from any peer.
#[derive(Debug)]
pub struct BlockAssembler {
    piece_count: u32,
    piece_length: u32,
    last_piece_length: u32,
    max_buffered: u32,
    assembled: PieceField,
    partial: BTreeMap<u32, PartialPiece>,
    completed: u32,
}

impl BlockAssembler {
    /// An assembler for a torrent of `piece_count` pieces: every piece but
    /// the last is `piece_length` bytes long, and the last one ends at
    /// `total_length`.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the layout is inconsistent (zero
    /// pieces, zero piece length, a last piece of zero or over
    /// `piece_length` bytes, or counts beyond the guards
    /// [`MAX_PIECES`]/[`MAX_BUFFERED_PIECES`]).
    pub fn new(
        piece_count: u32,
        piece_length: u32,
        total_length: u64,
        max_buffered: u32,
    ) -> Result<Self> {
        validate_piece_count(piece_count)?;
        if piece_length == 0 {
            return Err(Error::Download(
                "piece length must be at least one byte".into(),
            ));
        }
        if max_buffered == 0 || max_buffered > MAX_BUFFERED_PIECES {
            return Err(Error::Download(format!(
                "partial-piece cap {max_buffered} is outside 1..={MAX_BUFFERED_PIECES}"
            )));
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
            max_buffered,
            assembled: PieceField::new(piece_count)?,
            partial: BTreeMap::new(),
            completed: 0,
        })
    }

    /// The piece count this torrent is laid out for.
    #[must_use]
    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// The length of every piece but the last, in bytes.
    #[must_use]
    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// The length of the last piece, in bytes (1..=`piece_length`).
    #[must_use]
    pub fn last_piece_length(&self) -> u32 {
        self.last_piece_length
    }

    /// The total payload length this layout sums to.
    #[must_use]
    pub fn total_length(&self) -> u64 {
        u64::from(self.piece_count - 1) * u64::from(self.piece_length)
            + u64::from(self.last_piece_length)
    }

    /// The configured partial-piece cap.
    #[must_use]
    pub fn max_buffered(&self) -> u32 {
        self.max_buffered
    }

    /// How many pieces have been fully assembled so far.
    #[must_use]
    pub fn pieces_completed(&self) -> u32 {
        self.completed
    }

    /// Whether piece `index` has already been assembled; out-of-range
    /// indexes read `false` (queries never panic).
    #[must_use]
    pub fn is_assembled(&self, index: u32) -> bool {
        self.assembled.has(index)
    }

    /// The request blocks that tile piece `index`, in ascending order.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `index` is out of range or the
    /// piece is already assembled — planning requests for bytes we hold
    /// is a caller bug worth surfacing.
    pub fn block_plan(&self, index: u32) -> Result<Vec<BlockPlan>> {
        let length = self.piece_length_of(index)?;
        let mut plans = Vec::new();
        let mut begin = 0_u32;
        while begin < length {
            let block_length = block_length_at(length, begin);
            plans.push(BlockPlan {
                index,
                begin,
                length: block_length,
            });
            // Overflow-proof advance: when begin + BLOCK_LEN would wrap,
            // the just-pushed block already reaches the piece's end.
            begin = match begin.checked_add(BLOCK_LEN) {
                Some(next) => next,
                None => break,
            };
        }
        Ok(plans)
    }

    /// Files one received block of a piece; blocks may arrive in any
    /// order from any peer.
    ///
    /// Returns the fully assembled piece bytes the first time the last
    /// missing block of that piece lands.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the index is out of range, the
    /// piece is already assembled, the offset is misaligned or beyond the
    /// piece, the block does not match the planned length, the block is a
    /// duplicate, or the partial-piece cap ([`MAX_BUFFERED_PIECES`],
    /// configured via [`BlockAssembler::new`]) is full.
    pub fn push_block(
        &mut self,
        index: u32,
        begin: u32,
        block: &[u8],
    ) -> Result<Option<(u32, Vec<u8>)>> {
        let length = self.piece_length_of(index)?;
        if begin >= length {
            return Err(Error::Download(format!(
                "block begin {begin} lies beyond piece {index}'s {length} bytes"
            )));
        }
        if begin % BLOCK_LEN != 0 {
            return Err(Error::Download(format!(
                "block begin {begin} is not {BLOCK_LEN}-byte aligned"
            )));
        }
        let planned = block_length_at(length, begin);
        if block.len() != planned as usize {
            return Err(Error::Download(format!(
                "piece {index} block at {begin} is {} bytes, the layout plans {planned}",
                block.len()
            )));
        }
        let expected = block_count(length);
        let is_new = !self.partial.contains_key(&index);
        if is_new && self.partial.len() >= self.max_buffered as usize {
            return Err(Error::Download(format!(
                "too many partial pieces in flight (cap {})",
                self.max_buffered
            )));
        }
        let partial = self.partial.entry(index).or_insert(PartialPiece {
            blocks: BTreeMap::new(),
            expected,
        });
        if partial.blocks.contains_key(&begin) {
            return Err(Error::Download(format!(
                "piece {index} block at {begin} was already received"
            )));
        }
        partial.blocks.insert(begin, block.to_vec());
        if partial.blocks.len() == partial.expected as usize {
            let mut piece = vec![0_u8; length as usize];
            for (offset, data) in &partial.blocks {
                let start = *offset as usize;
                piece[start..start + data.len()].copy_from_slice(data);
            }
            self.partial.remove(&index);
            // In range by the check above: a typed error, never a panic.
            self.assembled.set_have(index)?;
            self.completed += 1;
            return Ok(Some((index, piece)));
        }
        Ok(None)
    }

    /// Routes one decoded wire message into the assembler.
    ///
    /// Only [`Message::Piece`] carries data; every other message id is a
    /// typed error because the session routes it elsewhere (state changes
    /// to the picker, cancels to the request side).
    ///
    /// # Errors
    /// Returns [`Error::Download`] for any non-`Piece` message and for
    /// everything [`BlockAssembler::push_block`] rejects.
    pub fn push_message(&mut self, message: &Message<'_>) -> Result<Option<(u32, Vec<u8>)>> {
        match message {
            Message::Piece {
                index,
                begin,
                block,
            } => self.push_block(*index, *begin, block),
            Message::KeepAlive => Err(only_pieces("keep-alive")),
            Message::Choke => Err(only_pieces("choke")),
            Message::Unchoke => Err(only_pieces("unchoke")),
            Message::Interested => Err(only_pieces("interested")),
            Message::NotInterested => Err(only_pieces("not-interested")),
            Message::Have(_) => Err(only_pieces("have")),
            Message::Bitfield(_) => Err(only_pieces("bitfield")),
            Message::Request { .. } => Err(only_pieces("request")),
            Message::Cancel { .. } => Err(only_pieces("cancel")),
            Message::Port(_) => Err(only_pieces("port")),
            Message::Unknown { .. } => Err(only_pieces("unknown")),
        }
    }

    /// Length of piece `index` and the range/assembly checks shared by
    /// planning and filing.
    fn piece_length_of(&self, index: u32) -> Result<u32> {
        if index >= self.piece_count {
            return Err(Error::Download(format!(
                "piece {index} is beyond the {}-piece torrent",
                self.piece_count
            )));
        }
        if self.assembled.has(index) {
            return Err(Error::Download(format!(
                "piece {index} is already assembled"
            )));
        }
        Ok(if index == self.piece_count - 1 {
            self.last_piece_length
        } else {
            self.piece_length
        })
    }
}

/// The block length at `begin` inside a piece of `length` bytes:
/// [`BLOCK_LEN`] except for the piece's final block.
fn block_length_at(length: u32, begin: u32) -> u32 {
    u32::min(BLOCK_LEN, length - begin)
}

/// How many blocks tile a piece of `length` bytes.
fn block_count(length: u32) -> u32 {
    length.div_ceil(BLOCK_LEN)
}

/// Canonical wire length of a bitfield over `piece_count` pieces.
fn wire_len(piece_count: u32) -> usize {
    piece_count.div_ceil(8) as usize
}

/// The set bit for piece `index` inside its wire byte.
fn bit_mask(index: u32) -> u8 {
    1 << (7 - (index % 8))
}

/// Shared guard: a torrent has between 1 and [`MAX_PIECES`] pieces.
fn validate_piece_count(piece_count: u32) -> Result<()> {
    if piece_count == 0 {
        return Err(Error::Download("a torrent has at least one piece".into()));
    }
    if piece_count > MAX_PIECES {
        return Err(Error::Download(format!(
            "piece count {piece_count} exceeds the {MAX_PIECES}-piece guard"
        )));
    }
    Ok(())
}

fn only_pieces(received: &str) -> Error {
    Error::Download(format!(
        "the assembler accepts only piece messages, got {received}"
    ))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    const OK_CAP: u32 = 8;

    fn assembler(piece_count: u32, piece_length: u32, total_length: u64) -> BlockAssembler {
        BlockAssembler::new(piece_count, piece_length, total_length, OK_CAP).unwrap()
    }

    #[test]
    fn constants_pin_the_downloader_guards() {
        assert_eq!(BLOCK_LEN, 16_384);
        assert_eq!(MAX_PIECES, 16_777_216);
        assert_eq!(MAX_BUFFERED_PIECES, 256);
    }

    #[test]
    fn piece_field_rejects_zero_and_oversized_counts() {
        assert!(PieceField::new(0).is_err());
        assert!(PieceField::new(MAX_PIECES).is_ok());
        assert!(PieceField::new(MAX_PIECES + 1).is_err());
        assert!(PieceField::from_bitfield(&[], 0).is_err());
        assert!(Availability::new(0).is_err());
    }

    #[test]
    fn piece_field_round_trips_through_the_wire_form() {
        let mut field = PieceField::new(130).unwrap();
        for index in [0, 7, 127, 129] {
            field.set_have(index).unwrap();
        }
        let wire = field.to_bitfield();
        assert_eq!(wire.len(), 17);
        assert_eq!(PieceField::from_bitfield(&wire, 130).unwrap(), field);
        assert!(field.has(0));
        assert!(field.has(129));
        assert!(!field.has(1));
        assert_eq!(field.have_count(), 4);
        assert_eq!(field.missing_count(), 126);
        assert!(!field.is_complete());
    }

    #[test]
    fn piece_field_accepts_short_and_canonical_wire_forms() {
        // A single byte naming piece 0 for a 130-piece torrent.
        let field = PieceField::from_bitfield(&[0b1000_0000], 130).unwrap();
        assert!(field.has(0));
        assert_eq!(field.have_count(), 1);
        // Canonical length with all-zero padding bits: pieces 0..8 of 9.
        let field = PieceField::from_bitfield(&[0xFF, 0b1000_0000], 9).unwrap();
        assert!(field.is_complete());
        // A set padding bit beyond the piece count is a shape violation.
        let err = PieceField::from_bitfield(&[0xFF, 0b0000_0001], 9).unwrap_err();
        assert!(err.to_string().contains("beyond the 9-piece"));
    }

    #[test]
    fn piece_field_rejects_oversized_wire_forms() {
        let err = PieceField::from_bitfield(&[0_u8; 3], 9).unwrap_err();
        assert!(err.to_string().contains("allows 2"));
    }

    #[test]
    fn piece_field_counts_and_first_missing() {
        let mut field = PieceField::new(24).unwrap();
        for index in [3, 4, 23] {
            field.set_have(index).unwrap();
        }
        assert_eq!(field.have_count(), 3);
        assert_eq!(field.missing_count(), 21);
        assert_eq!(field.first_missing(), Some(0));
        let have: Vec<u32> = field.iter_have().collect();
        assert_eq!(have, vec![3, 4, 23]);
        let missing: Vec<u32> = field.iter_missing().collect();
        assert_eq!(missing.first(), Some(&0));
        assert_eq!(missing.len(), 21);
        for index in 0..24 {
            field.set_have(index).unwrap();
        }
        assert!(field.is_complete());
        assert_eq!(field.first_missing(), None);
    }

    #[test]
    fn piece_field_queries_never_panic_on_out_of_range() {
        let mut field = PieceField::new(24).unwrap();
        assert!(!field.has(24));
        assert!(!field.has(u32::MAX));
        assert!(field.set_have(24).is_err());
    }

    #[test]
    fn availability_counts_peers_and_haves() {
        let mut peer_a = PieceField::new(8).unwrap();
        let mut peer_b = PieceField::new(8).unwrap();
        peer_a.set_have(0).unwrap();
        peer_a.set_have(1).unwrap();
        peer_b.set_have(1).unwrap();
        peer_b.set_have(7).unwrap();

        let mut availability = Availability::new(8).unwrap();
        availability.add_peer(&peer_a);
        assert_eq!(availability.count(0), 1);
        assert_eq!(availability.count(1), 1);
        availability.add_peer(&peer_b);
        assert_eq!(availability.count(1), 2);
        assert_eq!(availability.count(7), 1);

        availability.update(3).unwrap();
        assert_eq!(availability.count(3), 1);
        assert!(availability.update(8).is_err());

        availability.remove_peer(&peer_a);
        assert_eq!(availability.count(0), 0);
        assert_eq!(availability.count(1), 1);
    }

    #[test]
    fn picker_sequential_selects_the_lowest_piece_the_peer_has() {
        let mut ours = PieceField::new(4).unwrap();
        let mut theirs = PieceField::new(4).unwrap();
        ours.set_have(1).unwrap();
        theirs.set_have(0).unwrap();
        theirs.set_have(1).unwrap();
        theirs.set_have(2).unwrap();

        let picker = PiecePicker::new(PieceStrategy::Sequential);
        assert_eq!(picker.strategy(), PieceStrategy::Sequential);
        assert_eq!(picker.pick(&ours, &theirs, None), Some(0));

        ours.set_have(0).unwrap();
        assert_eq!(picker.pick(&ours, &theirs, None), Some(2));

        ours.set_have(2).unwrap();
        assert_eq!(picker.pick(&ours, &theirs, None), None);
        assert_eq!(PiecePicker::default().strategy(), PieceStrategy::Sequential);
    }

    #[test]
    fn picker_rarest_first_prefers_scarcer_pieces_with_lowest_index_ties() {
        let ours = PieceField::new(3).unwrap();
        let mut theirs = PieceField::new(3).unwrap();
        theirs.set_have(0).unwrap();
        theirs.set_have(1).unwrap();
        theirs.set_have(2).unwrap();

        let mut availability = Availability::new(3).unwrap();
        availability.update(1).unwrap();
        availability.update(1).unwrap();
        availability.update(1).unwrap();
        availability.update(2).unwrap();

        let picker = PiecePicker::new(PieceStrategy::RarestFirst);
        // Pieces 0 and 2 tie at one peer: the lowest index wins.
        assert_eq!(picker.pick(&ours, &theirs, Some(&availability)), Some(0));
        availability.update(0).unwrap();
        availability.update(0).unwrap();
        // Piece 2 is now the scarcest.
        assert_eq!(picker.pick(&ours, &theirs, Some(&availability)), Some(2));
        // Without availability the strategy degrades to sequential.
        assert_eq!(picker.pick(&ours, &theirs, None), Some(0));
    }

    #[test]
    fn assembler_rejects_inconsistent_layouts() {
        assert!(BlockAssembler::new(0, 16_384, 16_384, OK_CAP).is_err());
        assert!(BlockAssembler::new(2, 0, 32_768, OK_CAP).is_err());
        // Total length leaves a zero-byte last piece.
        assert!(BlockAssembler::new(2, 16_384, 16_384, OK_CAP).is_err());
        // Total length leaves a last piece longer than `piece_length`.
        assert!(BlockAssembler::new(2, 16_384, 40_000, OK_CAP).is_err());
        assert!(BlockAssembler::new(1, 16_384, 16_384, 0).is_err());
        assert!(BlockAssembler::new(1, 16_384, 16_384, MAX_BUFFERED_PIECES + 1).is_err());
        // A consistent layout: 2 pieces, last one 3_616 bytes.
        let assembler = assembler(2, 16_384, 20_000);
        assert_eq!(assembler.piece_count(), 2);
        assert_eq!(assembler.piece_length(), 16_384);
        assert_eq!(assembler.last_piece_length(), 3_616);
        assert_eq!(assembler.total_length(), 20_000);
        assert_eq!(assembler.max_buffered(), OK_CAP);
        assert_eq!(assembler.pieces_completed(), 0);
    }

    #[test]
    fn assembler_plans_blocks_over_pieces() {
        let assembler = assembler(3, 40_000, 90_000);
        let plan = assembler.block_plan(0).unwrap();
        assert_eq!(
            plan,
            vec![
                BlockPlan {
                    index: 0,
                    begin: 0,
                    length: 16_384
                },
                BlockPlan {
                    index: 0,
                    begin: 16_384,
                    length: 16_384
                },
                BlockPlan {
                    index: 0,
                    begin: 32_768,
                    length: 7_232
                },
            ]
        );
        // The last piece is 10_000 bytes: a single block.
        let plan = assembler.block_plan(2).unwrap();
        assert_eq!(
            plan,
            vec![BlockPlan {
                index: 2,
                begin: 0,
                length: 10_000
            }]
        );
        assert!(assembler.block_plan(3).is_err());
    }

    #[test]
    fn assembler_plans_one_block_for_pieces_below_the_block_size() {
        let assembler = assembler(3, 1, 3);
        for index in 0..3_u32 {
            let plan = assembler.block_plan(index).unwrap();
            assert_eq!(
                plan,
                vec![BlockPlan {
                    index,
                    begin: 0,
                    length: 1
                }]
            );
        }
    }

    #[test]
    fn assembler_assembles_pieces_from_out_of_order_blocks() {
        let mut assembler = assembler(2, 20_000, 40_000);
        let filler = |piece: u32, offset: u32| {
            u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251).unwrap_or(0)
        };
        let block = |piece: u32, begin: u32, length: u32| -> Vec<u8> {
            (0..length)
                .map(|step| filler(piece, begin + step))
                .collect()
        };
        // Second block of piece 0 first: no completion yet.
        assert_eq!(
            assembler
                .push_block(0, 16_384, &block(0, 16_384, 3_616))
                .unwrap(),
            None
        );
        assert!(!assembler.is_assembled(0));
        // First block completes the piece with the exact bytes fed in.
        let expected = block(0, 0, 16_384)
            .into_iter()
            .chain(block(0, 16_384, 3_616))
            .collect::<Vec<u8>>();
        let (index, piece) = assembler
            .push_block(0, 0, &block(0, 0, 16_384))
            .unwrap()
            .unwrap();
        assert_eq!(index, 0);
        assert_eq!(piece, expected);
        assert_eq!(assembler.pieces_completed(), 1);
        // The assembled piece is closed for business.
        assert!(assembler.is_assembled(0));
        assert!(assembler.push_block(0, 0, &block(0, 0, 16_384)).is_err());
        assert!(assembler.block_plan(0).is_err());
        // Piece 1 (the 20_000-byte last piece) completes the torrent.
        assert_eq!(
            assembler
                .push_block(1, 16_384, &block(1, 16_384, 3_616))
                .unwrap(),
            None
        );
        let (index, piece) = assembler
            .push_block(1, 0, &block(1, 0, 16_384))
            .unwrap()
            .unwrap();
        assert_eq!(index, 1);
        assert_eq!(piece.len(), 20_000);
        assert_eq!(assembler.pieces_completed(), 2);
    }

    #[test]
    fn assembler_enforces_the_partial_piece_cap() {
        let mut assembler = BlockAssembler::new(4, 40_000, 160_000, 2).unwrap();
        let block = vec![0x5A_u8; 16_384];
        assert_eq!(assembler.push_block(0, 0, &block).unwrap(), None);
        assert_eq!(assembler.push_block(1, 0, &block).unwrap(), None);
        // The cap of 2 partial pieces is full.
        let err = assembler.push_block(2, 0, &block).unwrap_err();
        assert!(err.to_string().contains("cap 2"));
        // Completing piece 0 (3 blocks) frees its slot.
        assert_eq!(assembler.push_block(0, 16_384, &block).unwrap(), None);
        let (index, piece) = assembler
            .push_block(0, 32_768, &vec![0x5A_u8; 40_000 - 2 * 16_384])
            .unwrap()
            .unwrap();
        assert_eq!(index, 0);
        assert_eq!(piece.len(), 40_000);
        // A duplicate of an already-received block is an error.
        assert!(assembler.push_block(1, 0, &block).is_err());
        // The freed slot admits piece 3.
        assert_eq!(assembler.push_block(3, 0, &block).unwrap(), None);
        assert_eq!(assembler.pieces_completed(), 1);
    }

    #[test]
    fn assembler_rejects_hostile_blocks() {
        let mut assembler = assembler(2, 16_384, 20_000);
        let block = vec![0x5A_u8; 16_384];
        assert!(assembler.push_block(2, 0, &block).is_err());
        assert!(assembler.push_block(0, 1, &block).is_err());
        assert!(assembler.push_block(0, 16_384, &block).is_err());
        let short = vec![0x5A_u8; 8];
        assert!(assembler.push_block(0, 0, &short).is_err());
        let long = vec![0x5A_u8; 17_000];
        assert!(assembler.push_block(0, 0, &long).is_err());
    }

    #[test]
    fn assembler_accepts_only_piece_messages() {
        let mut assembler = assembler(1, 16_384, 16_384);
        let block = vec![0x5A_u8; 16_384];
        assert!(assembler.push_message(&Message::Choke).is_err());
        assert!(assembler.push_message(&Message::KeepAlive).is_err());
        assert!(
            assembler
                .push_message(&Message::Unknown {
                    id: 20,
                    payload: Cow::Borrowed(&[][..])
                })
                .is_err()
        );
        let message = Message::Piece {
            index: 0,
            begin: 0,
            block: Cow::Borrowed(&block[..]),
        };
        assert!(assembler.push_message(&message).unwrap().is_some());
    }

    #[test]
    fn block_plans_encode_to_request_messages() {
        let plan = BlockPlan {
            index: 9,
            begin: 65_536,
            length: 16_384,
        };
        assert_eq!(
            plan.to_message(),
            Message::Request {
                index: 9,
                begin: 65_536,
                length: 16_384,
            }
        );
    }
}
