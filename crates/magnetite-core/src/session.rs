//! Runtime-free per-connection session bookkeeping: the outbound request
//! pipeline and the message router the downloader deferred to.
//!
//! This closes the seam every shipped module documents:
//! [`crate::downloader::BlockAssembler::push_message`] rejects every
//! non-`Piece` message "because the session routes it elsewhere (state
//! changes to the picker, cancels to the request side)", and the downloader's
//! module contract states that "request bookkeeping (pipeline depth, cancels)
//! stays on the per-connection side" — this module is that per-connection
//! side. [`SessionPump`] wires the four runtime-free stages into one
//! per-peer pump: [`crate::downloader::BlockAssembler`] (blocks),
//! [`crate::verify::PieceVerifier`] (verdicts), a [`RequestPipeline`]
//! (in-flight requests under a depth cap), and swarm bookkeeping
//! ([`crate::downloader::Availability`], [`crate::downloader::PieceField`])
//! that turns `Bitfield`/`Have`/`Choke`/`Unchoke` messages into picker state.
//!
//! The future `tokio` stream session stays a thin pump: for every decoded
//! [`crate::peer_wire::Message`] it calls [`SessionPump::handle_message`],
//! and for every outbound slot it calls [`SessionPump::next_request`], then
//! writes [`crate::downloader::BlockPlan::to_message`] to the socket.
//! Nothing here knows a socket exists.
//!
//! House rules, unchanged from the codec, tracker, peer wire, downloader, and
//! verification edges:
//!
//! - Adversarial input is *data*: a piece whose bytes hash differently is an
//!   `Ok(SessionEvent::PieceRejected)` verdict, while protocol-shape
//!   violations (malformed bitfields, out-of-range `Have`s, duplicate or
//!   depth-capped requests) are typed [`Error`] values. Nothing panics.
//! - Byte strings stay borrowed: the expected digests are borrowed from the
//!   caller's metainfo (`SessionPump<'a>`), exactly like the verifier's.
//! - The module is runtime-free: no I/O, no clocks, no executor; `tracing`
//!   spans are no-ops until a subscriber is installed, and embedders pay
//!   nothing for them.
//!
//! ```no_run
//! use magnetite_core::downloader::PieceStrategy;
//! use magnetite_core::session::{SessionEvent, SessionPump};
//! use std::borrow::Cow;
//!
//! # fn main() -> Result<(), magnetite_core::Error> {
//! # let (hashes, piece_count, piece_length, total_length) = (&[0_u8; 20], 1_u32, 16_384_u32, 16_384_u64);
//! let mut pump = SessionPump::new(
//!     hashes, piece_count, piece_length, total_length, 8, 250, PieceStrategy::RarestFirst,
//! )?;
//!
//! // The peer's bitfield arrives; the pump updates swarm bookkeeping.
//! # let wire = [0b1000_0000_u8];
//! let event = pump
//!     .handle_message(&magnetite_core::peer_wire::Message::Bitfield(Cow::Borrowed(&wire)))?;
//!
//! // Outbound: take a planned request while the pipeline has room.
//! if let Some(plan) = pump.next_request()? {
//!     let _request = plan.to_message();
//! }
//! # let _ = event;
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeMap, BTreeSet};

use crate::downloader::{
    Availability, BLOCK_LEN, BlockAssembler, BlockPlan, PieceField, PieceStrategy,
};
use crate::error::{Error, Result};
use crate::peer_wire::Message;
use crate::verify::PieceVerifier;

/// Upper bound for the per-connection pipeline depth, 250 in-flight block
/// requests.
///
/// BEP 3 practice caps a client's outstanding requests well below what a
/// 16 KiB block size implies; 250 is the de-facto ceiling mainstream clients
/// agree on, and it bounds the memory a hostile peer can make a session
/// waste by ignoring replies.
pub const MAX_PIPELINE_DEPTH: u32 = 250;

/// The per-connection outbound request bookkeeping the downloader deferred.
///
/// Tracks which blocks this connection has asked for and not yet received,
/// under an explicit depth cap: a peer that never answers cannot grow the
/// tracker of outstanding requests beyond [`MAX_PIPELINE_DEPTH`]. `Choke`
/// evicts every in-flight request (the peer will not serve them), and each
/// incoming `Piece` releases exactly the matching request.
#[derive(Debug)]
pub struct RequestPipeline {
    max_depth: u32,
    choked: bool,
    depth: u32,
    in_flight: BTreeMap<u32, BTreeMap<u32, BlockPlan>>,
}

impl RequestPipeline {
    /// A pipeline holding at most `max_depth` requests in flight.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when `max_depth` is zero or exceeds
    /// [`MAX_PIPELINE_DEPTH`].
    pub fn new(max_depth: u32) -> Result<Self> {
        if max_depth == 0 || max_depth > MAX_PIPELINE_DEPTH {
            return Err(Error::Download(format!(
                "pipeline depth {max_depth} is outside 1..={MAX_PIPELINE_DEPTH}"
            )));
        }
        Ok(Self {
            max_depth,
            choked: false,
            depth: 0,
            in_flight: BTreeMap::new(),
        })
    }

    /// The configured in-flight cap.
    #[must_use]
    pub const fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// How many requests are in flight right now.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// Whether the peer has choked this connection's downloads.
    #[must_use]
    pub const fn is_choked(&self) -> bool {
        self.choked
    }

    /// Whether the exact block `(index, begin)` is in flight.
    #[must_use]
    pub fn is_in_flight(&self, index: u32, begin: u32) -> bool {
        self.in_flight
            .get(&index)
            .is_some_and(|blocks| blocks.contains_key(&begin))
    }

    /// Whether no request is in flight right now.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.depth == 0
    }

    /// Registers one outbound request.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the connection is choked, the
    /// pipeline is at its depth cap, or the exact block is already in
    /// flight — re-requesting without a cancel is a caller bug worth
    /// surfacing, matching the assembler's stance on duplicates.
    pub fn record_request(&mut self, plan: &BlockPlan) -> Result<()> {
        if self.choked {
            return Err(Error::Download(format!(
                "peer is choked; refusing to request piece {} at {}",
                plan.index, plan.begin
            )));
        }
        if self.depth >= self.max_depth {
            return Err(Error::Download(format!(
                "pipeline is at its {}-request cap",
                self.max_depth
            )));
        }
        if self.is_in_flight(plan.index, plan.begin) {
            return Err(Error::Download(format!(
                "piece {} block at {} is already in flight",
                plan.index, plan.begin
            )));
        }
        self.in_flight
            .entry(plan.index)
            .or_default()
            .insert(plan.begin, *plan);
        self.depth += 1;
        Ok(())
    }

    /// Retires one in-flight request — its block arrived (served) or the
    /// caller canceled it. Returns whether it actually was in flight.
    pub fn release(&mut self, index: u32, begin: u32) -> bool {
        let Some(blocks) = self.in_flight.get_mut(&index) else {
            return false;
        };
        if blocks.remove(&begin).is_none() {
            return false;
        }
        if blocks.is_empty() {
            self.in_flight.remove(&index);
        }
        self.depth -= 1;
        true
    }

    /// The peer choked us: every in-flight request is dead on arrival, so
    /// all of them are evicted and returned (ordered by piece, then offset)
    /// for the caller to re-route to other peers.
    pub fn on_choke(&mut self) -> Vec<BlockPlan> {
        self.choked = true;
        let evicted = self
            .in_flight
            .values()
            .flat_map(|blocks| blocks.values())
            .copied()
            .collect();
        self.depth = 0;
        self.in_flight.clear();
        evicted
    }

    /// The peer unchoked us: new requests are accepted again.
    pub fn on_unchoke(&mut self) {
        self.choked = false;
    }
}

/// What one routed wire message did to the session.
///
/// Data events (`PieceVerified`, `PieceRejected`) hand the caller exactly
/// what the pump owes the rest of the stack — verified bytes for storage,
/// rejected indexes for re-request bookkeeping. Control events describe
/// what the peer's state change did to the pipeline and the swarm
/// bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A block was buffered into a piece still missing blocks (`pending`
    /// counts the blocks that must still arrive before assembly).
    BlockBuffered {
        /// Piece the block landed in.
        index: u32,
        /// Blocks still missing from this piece.
        pending: u32,
    },
    /// A piece assembled and its SHA-1 matched the metainfo digest; the
    /// bytes are the caller's to persist.
    PieceVerified {
        /// The verified piece index.
        index: u32,
        /// The full piece bytes, ready for storage.
        bytes: Vec<u8>,
    },
    /// A piece assembled but hashed differently — data, not a crash: the
    /// bytes are dropped, the piece is recorded in
    /// [`SessionPump::rejected`] and never picked again until the
    /// re-request loop opens through [`SessionPump::retry_rejected`].
    PieceRejected {
        /// The rejected piece index.
        index: u32,
    },
    /// The peer's opening bitfield was accepted; `have_count` pieces are
    /// now offered.
    PeerBitfield {
        /// How many pieces the peer claims to have.
        have_count: u32,
    },
    /// The peer announced one more piece via `Have`.
    PeerHave {
        /// The newly-announced piece index.
        index: u32,
    },
    /// The peer choked us: every in-flight request was evicted and is
    /// listed here for re-routing to other peers.
    PeerChoked {
        /// The requests that died with the choke.
        evicted: Vec<BlockPlan>,
    },
    /// The peer unchoked us; [`SessionPump::next_request`] resumes.
    PeerUnchoked,
    /// The peer canceled one of its uploads to us; the session uploads
    /// nothing yet, so this is bookkeeping only.
    PeerCancel {
        /// Piece the peer stopped wanting.
        index: u32,
        /// Offset inside the piece.
        begin: u32,
    },
    /// `KeepAlive`, `Interested`, `NotInterested`, `Port`, and unknown
    /// message ids: routed to nothing (unknown ids stay preserved at the
    /// codec layer for extension round-trips).
    Ignored,
}

/// The runtime-free per-peer session: routes wire messages into the
/// assembler, verifier, and swarm bookkeeping, and plans outbound requests
/// under a pipeline cap.
///
/// One pump serves exactly one peer connection: `theirs` is that peer's
/// claimed pieces, `availability` carries this connection's contribution to
/// the swarm counts, and the [`RequestPipeline`] tracks what this
/// connection asked for and never received. The pump consumes the
/// [`PieceStrategy`] for candidate ordering — sequential walks the peer's
/// have-list ascending, rarest-first prefers the scarcest pieces first
/// (ties to the lowest index), mirroring [`crate::downloader::PiecePicker`].
#[derive(Debug)]
pub struct SessionPump<'a> {
    assembler: BlockAssembler,
    verifier: PieceVerifier<'a>,
    pipeline: RequestPipeline,
    availability: Availability,
    strategy: PieceStrategy,
    /// Pieces no longer pickable: verified (held) or rejected (burned in
    /// the assembler — re-assembly needs the re-request loop, reachable
    /// through [`SessionPump::retry_rejected`]).
    ours: PieceField,
    /// The peer's claimed pieces (bitfield + haves).
    theirs: PieceField,
    /// Whether the peer already sent its (optional, once-per-connection)
    /// bitfield: a second one is a protocol-shape violation, not a reason
    /// to double-count swarm availability.
    bitfield_seen: bool,
    /// Blocks already received, per piece still missing blocks; keeps
    /// `next_request` from re-requesting served blocks.
    served: BTreeMap<u32, BTreeSet<u32>>,
    /// Piece indexes whose bytes failed verification.
    rejected: Vec<u32>,
}

impl<'a> SessionPump<'a> {
    /// A pump for one peer on a torrent of `piece_count` pieces (every
    /// piece but the last `piece_length` bytes long, ending at
    /// `total_length`), with `max_buffered` partial pieces and a
    /// `pipeline_depth` in-flight cap.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the layout is inconsistent (see
    /// [`BlockAssembler::new`]), the digests disagree with the piece count
    /// (see [`PieceVerifier::new`]), or the pipeline depth is outside
    /// `1..=[MAX_PIPELINE_DEPTH]` (see [`RequestPipeline::new`]).
    pub fn new(
        pieces: &'a [u8],
        piece_count: u32,
        piece_length: u32,
        total_length: u64,
        max_buffered: u32,
        pipeline_depth: u32,
        strategy: PieceStrategy,
    ) -> Result<Self> {
        let assembler = BlockAssembler::new(piece_count, piece_length, total_length, max_buffered)?;
        let verifier = PieceVerifier::new(pieces, piece_count)?;
        let pipeline = RequestPipeline::new(pipeline_depth)?;
        let availability = Availability::new(piece_count)?;
        let ours = PieceField::new(piece_count)?;
        let theirs = PieceField::new(piece_count)?;
        Ok(Self {
            assembler,
            verifier,
            pipeline,
            availability,
            strategy,
            ours,
            theirs,
            bitfield_seen: false,
            served: BTreeMap::new(),
            rejected: Vec::new(),
        })
    }

    /// Routes one decoded wire message into the session.
    ///
    /// `Piece` payloads feed the assembler; an assembled piece is judged
    /// against the metainfo digests and surfaced as
    /// [`SessionEvent::PieceVerified`] (bytes included) or
    /// [`SessionEvent::PieceRejected`]. `Bitfield`/`Have` update this
    /// peer's claims and the availability counts, `Choke` evicts the
    /// pipeline, and everything else is control data the groundwork routes
    /// to nothing.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the message violates the torrent
    /// layout (malformed bitfield shape, out-of-range `Have` or `Piece`
    /// index, misaligned/duplicate block) or the assembler's guards.
    #[tracing::instrument(level = "debug", name = "session::handle_message", skip_all)]
    pub fn handle_message(&mut self, message: &Message<'_>) -> Result<SessionEvent> {
        match message {
            Message::Bitfield(wire) => {
                // BEP 3 sends the bitfield once, right after the handshake;
                // a second one is a hostile or broken peer. Guarding on the
                // accepted bitfield keeps swarm counts from being silently
                // double-counted — the rejection is data, not a state
                // change.
                if self.bitfield_seen {
                    return Err(Error::Download(
                        "peer already sent a bitfield; a second one is a protocol violation".into(),
                    ));
                }
                let claimed = PieceField::from_bitfield(wire, self.piece_count())?;
                self.availability.add_peer(&claimed);
                let have_count = claimed.have_count();
                self.theirs = claimed;
                self.bitfield_seen = true;
                tracing::debug!(have_count, "peer sent bitfield");
                Ok(SessionEvent::PeerBitfield { have_count })
            }
            Message::Have(index) => {
                self.theirs.set_have(*index)?;
                self.availability.update(*index)?;
                tracing::debug!(index, "peer announced have");
                Ok(SessionEvent::PeerHave { index: *index })
            }
            Message::Choke => {
                let evicted = self.pipeline.on_choke();
                tracing::debug!(count = evicted.len(), "peer choked, requests evicted");
                Ok(SessionEvent::PeerChoked { evicted })
            }
            Message::Unchoke => {
                self.pipeline.on_unchoke();
                tracing::debug!("peer unchoked");
                Ok(SessionEvent::PeerUnchoked)
            }
            Message::Cancel { index, begin, .. } => {
                tracing::debug!(index, begin, "peer canceled an upload");
                Ok(SessionEvent::PeerCancel {
                    index: *index,
                    begin: *begin,
                })
            }
            Message::Piece {
                index,
                begin,
                block,
            } => self.handle_piece(*index, *begin, block),
            Message::KeepAlive
            | Message::Interested
            | Message::NotInterested
            | Message::Port(_)
            | Message::Request { .. }
            | Message::Unknown { .. } => Ok(SessionEvent::Ignored),
        }
    }

    /// Plans the next outbound request, or `None` when the pipeline has no
    /// room, the peer is choking us, or nothing the peer offers is still
    /// worth asking for.
    ///
    /// Candidates are the peer's claimed pieces we do not hold (verified or
    /// rejected), ordered by the pump's [`PieceStrategy`]; within a piece,
    /// blocks already in flight or already served are skipped, so calling
    /// this repeatedly tiles the piece in [`BLOCK_LEN`] steps.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the internal request bookkeeping
    /// and the assembler's layout disagree — a caller bug worth surfacing.
    #[tracing::instrument(level = "debug", name = "session::next_request", skip_all)]
    pub fn next_request(&mut self) -> Result<Option<BlockPlan>> {
        if self.pipeline.is_choked() || self.pipeline.depth() >= self.pipeline.max_depth() {
            return Ok(None);
        }
        let mut candidates: Vec<u32> = self
            .theirs
            .iter_have()
            .filter(|index| !self.ours.has(*index))
            .collect();
        match self.strategy {
            PieceStrategy::Sequential => candidates.sort_unstable(),
            // Rarest-first: scarcest swarm count first, ties to the lowest
            // index, exactly like the standalone picker's ranking.
            PieceStrategy::RarestFirst => {
                candidates.sort_by_key(|index| (self.availability.count(*index), *index));
            }
        }
        for index in candidates {
            if self.assembler.is_assembled(index) {
                continue;
            }
            for plan in self.assembler.block_plan(index)? {
                if self.pipeline.is_in_flight(plan.index, plan.begin)
                    || self
                        .served
                        .get(&plan.index)
                        .is_some_and(|blocks| blocks.contains(&plan.begin))
                {
                    continue;
                }
                self.pipeline.record_request(&plan)?;
                tracing::debug!(
                    index = plan.index,
                    begin = plan.begin,
                    length = plan.length,
                    "request planned"
                );
                return Ok(Some(plan));
            }
        }
        Ok(None)
    }

    /// Cancels one of this connection's in-flight requests and returns the
    /// wire `Cancel` to send, or `None` when the block is not in flight
    /// (cancels race piece delivery; retiring an unknown block is a no-op).
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the in-flight bookkeeping names a
    /// block the assembler's layout does not plan — a caller bug worth
    /// surfacing.
    pub fn cancel_request(&mut self, index: u32, begin: u32) -> Result<Option<Message<'static>>> {
        if !self.pipeline.is_in_flight(index, begin) {
            return Ok(None);
        }
        let plan = self
            .assembler
            .block_plan(index)?
            .into_iter()
            .find(|plan| plan.begin == begin)
            .ok_or_else(|| {
                Error::Download(format!(
                    "in-flight piece {index} block at {begin} is not part of the layout"
                ))
            })?;
        self.pipeline.release(index, begin);
        tracing::debug!(index, begin, "request canceled");
        Ok(Some(Message::Cancel {
            index,
            begin,
            length: plan.length,
        }))
    }

    /// The per-connection request pipeline.
    #[must_use]
    pub const fn pipeline(&self) -> &RequestPipeline {
        &self.pipeline
    }

    /// The strategy the pump orders candidates by.
    #[must_use]
    pub const fn strategy(&self) -> PieceStrategy {
        self.strategy
    }

    /// The piece count this torrent is laid out for.
    #[must_use]
    pub fn piece_count(&self) -> u32 {
        self.assembler.piece_count()
    }

    /// How many pieces have passed verification so far.
    #[must_use]
    pub fn verified_count(&self) -> u32 {
        self.verifier.verified_count()
    }

    /// Whether every piece of the torrent has passed verification.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.verifier.is_complete()
    }

    /// The verified-pieces field, ready for the session remainder's
    /// have-list and the storage layer's completeness check.
    #[must_use]
    pub const fn verified_field(&self) -> &PieceField {
        self.verifier.verified_field()
    }

    /// The pieces this session no longer picks: verified or rejected.
    #[must_use]
    pub const fn ours_field(&self) -> &PieceField {
        &self.ours
    }

    /// The peer's claimed pieces (bitfield + haves).
    #[must_use]
    pub const fn theirs_field(&self) -> &PieceField {
        &self.theirs
    }

    /// This connection's contribution to the swarm availability counts.
    #[must_use]
    pub const fn availability(&self) -> &Availability {
        &self.availability
    }

    /// Piece indexes whose bytes failed verification, in rejection order.
    #[must_use]
    pub fn rejected(&self) -> &[u32] {
        &self.rejected
    }

    /// Reopens every piece whose bytes failed verification and returns the
    /// requeued indexes, in rejection order.
    ///
    /// This is the re-request loop the rejection verdict deferred to: a
    /// rejected piece was burned in the assembler and marked unpickable in
    /// [`SessionPump::ours_field`], so one corrupt delivery used to cap the
    /// download below completion forever. `retry_rejected` clears that
    /// state — the verifier marked nothing on a mismatch and the pipeline
    /// already released the piece's requests — so the pump plans the
    /// piece's blocks again and a clean delivery verifies as usual.
    /// Draining: a second call requeues nothing.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the rejection bookkeeping and the
    /// assembler's layout disagree — a caller bug worth surfacing.
    #[tracing::instrument(level = "debug", name = "session::retry_rejected", skip_all)]
    pub fn retry_rejected(&mut self) -> Result<Vec<u32>> {
        let requeued = std::mem::take(&mut self.rejected);
        for index in &requeued {
            self.assembler.reset_piece(*index)?;
            self.ours.clear_have(*index)?;
            tracing::debug!(index, "rejected piece requeued for re-request");
        }
        Ok(requeued)
    }

    /// Files one received block through the assembler and, when the piece
    /// assembles, through the verifier.
    fn handle_piece(&mut self, index: u32, begin: u32, block: &[u8]) -> Result<SessionEvent> {
        // The request is served the moment its bytes arrive — cancel races
        // and unsolicited pushes release the slot either way.
        self.pipeline.release(index, begin);
        let Some((index, bytes)) = self.assembler.push_block(index, begin, block)? else {
            // The block is filed into the partial piece; the pump must not
            // plan it again until the piece completes.
            self.served.entry(index).or_default().insert(begin);
            let pending = self.pending_blocks(index);
            tracing::debug!(index, pending, "block buffered");
            return Ok(SessionEvent::BlockBuffered { index, pending });
        };
        // Drop the served-block tracking: the piece is out of the pipeline
        // for good, verified or not.
        self.served.remove(&index);
        if self.verifier.verify_piece(index, &bytes)? {
            // In range by construction: a typed error, never a panic.
            self.ours.set_have(index)?;
            tracing::debug!(index, "piece verified");
            Ok(SessionEvent::PieceVerified { index, bytes })
        } else {
            // Mismatch is data: mark the piece unpickable (the assembler
            // has burned it) and record it for the re-request loop, which
            // opens through [`SessionPump::retry_rejected`].
            self.ours.set_have(index)?;
            self.rejected.push(index);
            tracing::debug!(index, "piece rejected by its digest");
            Ok(SessionEvent::PieceRejected { index })
        }
    }

    /// Blocks still missing from piece `index` — everything the layout
    /// plans minus what has already been served for it.
    fn pending_blocks(&self, index: u32) -> u32 {
        let length = if index == self.assembler.piece_count() - 1 {
            self.assembler.last_piece_length()
        } else {
            self.assembler.piece_length()
        };
        let planned = length.div_ceil(BLOCK_LEN);
        let received =
            u32::try_from(self.served.get(&index).map_or(0, BTreeSet::len)).unwrap_or(u32::MAX);
        planned.saturating_sub(received)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use sha1::{Digest, Sha1};

    use super::*;

    /// Fills `length` bytes deterministically: offset `o` of piece `p` is
    /// `(7p + o) % 251`, the same pattern the downloader and verifier specs
    /// use.
    fn piece_bytes(piece: u32, length: u32) -> Vec<u8> {
        (0..length)
            .map(|offset| {
                u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251).unwrap_or(0)
            })
            .collect()
    }

    /// Hashes pieces with an independent SHA-1 pass — a decoder-bug-style
    /// mistake inside the verifier cannot mask itself.
    fn hashes_for(pieces: &[Vec<u8>]) -> Vec<u8> {
        let mut hashes = Vec::new();
        for piece in pieces {
            let digest: [u8; 20] = Sha1::digest(piece).into();
            hashes.extend_from_slice(&digest);
        }
        hashes
    }

    /// The `(piece_count, piece_length, total_length)` layout of `pieces`.
    fn layout(pieces: &[Vec<u8>]) -> (u32, u32, u64) {
        let count = u32::try_from(pieces.len()).unwrap_or(0);
        let length = u32::try_from(pieces.first().map_or(0, Vec::len)).unwrap_or(0);
        let total = u64::try_from(pieces.iter().map(Vec::len).sum::<usize>()).unwrap_or(0);
        (count, length, total)
    }

    /// The wire bitfield in which every one of `count` pieces is set.
    fn full_bitfield(count: u32) -> Vec<u8> {
        let mut wire = PieceField::new(count).unwrap();
        for index in 0..count {
            wire.set_have(index).unwrap();
        }
        wire.to_bitfield()
    }

    /// Binds `hashes` and `pump` in the caller's scope — the pump borrows
    /// the digests, so the two bindings must live together for the whole
    /// test. `pieces` must already be in scope.
    macro_rules! seeded_pump {
        ($hashes:ident, $pump:ident, $pieces:ident, $depth:expr, $strategy:expr) => {{
            $hashes = hashes_for(&$pieces);
            let (count, length, total) = layout(&$pieces);
            $pump = SessionPump::new(&$hashes, count, length, total, 8, $depth, $strategy).unwrap();
            let wire = full_bitfield(count);
            let event = $pump
                .handle_message(&Message::Bitfield(Cow::Borrowed(&wire)))
                .unwrap();
            assert_eq!(event, SessionEvent::PeerBitfield { have_count: count });
        }};
    }

    /// Serves `plan` with the fake peer's bytes for that piece.
    fn serve(pieces: &[Vec<u8>], plan: &BlockPlan) -> Message<'static> {
        Message::Piece {
            index: plan.index,
            begin: plan.begin,
            block: Cow::Owned(
                pieces[plan.index as usize]
                    [plan.begin as usize..(plan.begin + plan.length) as usize]
                    .to_vec(),
            ),
        }
    }

    #[test]
    fn constants_pin_the_pipeline_layout() {
        assert_eq!(MAX_PIPELINE_DEPTH, 250);
    }

    #[test]
    fn pipeline_rejects_depth_outside_bounds() {
        let err = RequestPipeline::new(0).unwrap_err();
        assert!(err.to_string().contains("outside 1..=250"));
        let err = RequestPipeline::new(MAX_PIPELINE_DEPTH + 1).unwrap_err();
        assert!(err.to_string().contains("outside 1..=250"));
        assert!(RequestPipeline::new(1).is_ok());
        assert!(RequestPipeline::new(MAX_PIPELINE_DEPTH).is_ok());
    }

    #[test]
    fn pipeline_records_requests_and_refuses_duplicates_and_overflow() {
        let mut pipeline = RequestPipeline::new(2).unwrap();
        let plan = BlockPlan {
            index: 3,
            begin: 0,
            length: BLOCK_LEN,
        };
        pipeline.record_request(&plan).unwrap();
        assert_eq!(pipeline.depth(), 1);
        assert!(pipeline.is_in_flight(3, 0));
        assert!(!pipeline.is_in_flight(3, BLOCK_LEN));
        // The exact block twice is a caller bug.
        let err = pipeline.record_request(&plan).unwrap_err();
        assert!(err.to_string().contains("already in flight"));
        // Fill the cap.
        pipeline
            .record_request(&BlockPlan {
                index: 3,
                begin: BLOCK_LEN,
                length: BLOCK_LEN,
            })
            .unwrap();
        let err = pipeline
            .record_request(&BlockPlan {
                index: 4,
                begin: 0,
                length: BLOCK_LEN,
            })
            .unwrap_err();
        assert!(err.to_string().contains("2-request cap"));
    }

    #[test]
    fn pipeline_release_retires_in_flight_blocks() {
        let mut pipeline = RequestPipeline::new(4).unwrap();
        pipeline
            .record_request(&BlockPlan {
                index: 0,
                begin: 0,
                length: BLOCK_LEN,
            })
            .unwrap();
        assert!(pipeline.release(0, 0));
        assert!(!pipeline.is_in_flight(0, 0));
        assert_eq!(pipeline.depth(), 0);
        assert!(pipeline.is_empty());
        // Retiring an unknown block answers false, never panics.
        assert!(!pipeline.release(0, 0));
        assert!(!pipeline.release(9, 0));
    }

    #[test]
    fn pipeline_choke_evicts_everything_in_order() {
        let mut pipeline = RequestPipeline::new(4).unwrap();
        let first = BlockPlan {
            index: 1,
            begin: 0,
            length: BLOCK_LEN,
        };
        let second = BlockPlan {
            index: 1,
            begin: BLOCK_LEN,
            length: BLOCK_LEN,
        };
        let third = BlockPlan {
            index: 2,
            begin: 0,
            length: BLOCK_LEN,
        };
        for plan in [&first, &second, &third] {
            pipeline.record_request(plan).unwrap();
        }
        let evicted = pipeline.on_choke();
        assert_eq!(evicted, vec![first, second, third]);
        assert_eq!(pipeline.depth(), 0);
        assert!(pipeline.is_choked());
        assert!(pipeline.is_empty());
        // A choked connection refuses new requests.
        let err = pipeline.record_request(&first).unwrap_err();
        assert!(err.to_string().contains("choked"));
        pipeline.on_unchoke();
        assert!(!pipeline.is_choked());
        pipeline.record_request(&first).unwrap();
    }

    #[test]
    fn pump_updates_swarm_bookkeeping_from_bitfield_and_haves() {
        let pieces = vec![piece_bytes(0, 128), piece_bytes(1, 128)];
        let hashes = hashes_for(&pieces);
        let (count, length, total) = layout(&pieces);
        let mut pump = SessionPump::new(
            &hashes,
            count,
            length,
            total,
            8,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential,
        )
        .unwrap();
        // A bitfield with a stray high bit beyond the piece count is data.
        let err = pump
            .handle_message(&Message::Bitfield(Cow::Borrowed(&[0b1110_0000])))
            .unwrap_err();
        assert!(err.to_string().contains("beyond"));
        let event = pump
            .handle_message(&Message::Bitfield(Cow::Borrowed(&[0b1100_0000])))
            .unwrap();
        assert_eq!(event, SessionEvent::PeerBitfield { have_count: 2 });
        assert!(pump.theirs_field().has(0));
        assert!(pump.theirs_field().has(1));
        assert_eq!(pump.availability().count(0), 1);
        // Have 0 again: a redundant claim still routes into the swarm
        // counts — the saturating count is swarm data, and the pump is a
        // faithful router (mainstream clients only send Have for pieces
        // acquired after the bitfield, so the inflation is bounded).
        let event = pump.handle_message(&Message::Have(0)).unwrap();
        assert_eq!(event, SessionEvent::PeerHave { index: 0 });
        assert_eq!(pump.availability().count(0), 2);
        // Out-of-range haves are typed errors, never panics.
        assert!(pump.handle_message(&Message::Have(2)).is_err());
        // A second bitfield is a protocol-shape violation: a typed error,
        // and the swarm counts are not double-counted by the duplicate.
        let err = pump
            .handle_message(&Message::Bitfield(Cow::Borrowed(&[0b1100_0000])))
            .unwrap_err();
        assert!(err.to_string().contains("already sent a bitfield"));
        assert_eq!(pump.availability().count(0), 2);
        assert_eq!(pump.theirs_field().have_count(), 2);
    }

    #[test]
    fn pump_serves_a_happy_path_download_end_to_end() {
        let pieces = vec![
            piece_bytes(0, 128),
            piece_bytes(1, 128),
            piece_bytes(2, 128),
        ];
        let hashes;
        let mut pump;
        seeded_pump!(
            hashes,
            pump,
            pieces,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::RarestFirst
        );
        let mut served_events = 0;
        while let Some(plan) = pump.next_request().unwrap() {
            let event = pump.handle_message(&serve(&pieces, &plan)).unwrap();
            match event {
                SessionEvent::PieceVerified { index, bytes } => {
                    served_events += 1;
                    assert_eq!(bytes, pieces[index as usize]);
                }
                other => panic!("unexpected event during happy path: {other:?}"),
            }
        }
        assert_eq!(served_events, 3);
        assert_eq!(pump.verified_count(), 3);
        assert!(pump.is_complete());
        assert_eq!(pump.verified_field().to_bitfield(), &[0b1110_0000]);
        assert_eq!(pump.ours_field().to_bitfield(), &[0b1110_0000]);
        // Every request was retired with its bytes.
        assert_eq!(pump.pipeline().depth(), 0);
        assert!(pump.pipeline().is_empty());
        // The completed torrent offers nothing new.
        assert!(pump.next_request().unwrap().is_none());
    }

    #[test]
    fn pump_rejects_corrupt_pieces_as_data() {
        let pieces = vec![piece_bytes(0, 128), piece_bytes(1, 128)];
        let hashes;
        let mut pump;
        seeded_pump!(
            hashes,
            pump,
            pieces,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential
        );
        // Corrupt piece 0's only block.
        let mut corrupt = pieces[0].clone();
        corrupt[7] ^= 0x20;
        let event = pump
            .handle_message(&Message::Piece {
                index: 0,
                begin: 0,
                block: Cow::Owned(corrupt),
            })
            .unwrap();
        assert_eq!(event, SessionEvent::PieceRejected { index: 0 });
        assert_eq!(pump.rejected(), [0]);
        assert_eq!(pump.verified_count(), 0);
        assert!(!pump.is_complete());
        // The rejected piece is no longer pickable: only piece 1 is served.
        let plan = pump.next_request().unwrap().unwrap();
        assert_eq!(plan.index, 1);
        // Re-filing bytes for the burned piece is the assembler's typed error.
        let err = pump
            .handle_message(&Message::Piece {
                index: 0,
                begin: 0,
                block: Cow::Owned(pieces[0].clone()),
            })
            .unwrap_err();
        assert!(err.to_string().contains("already assembled"));
    }

    #[test]
    fn pump_requeues_rejected_pieces_for_a_clean_retry() {
        let pieces = vec![piece_bytes(0, 128), piece_bytes(1, 128)];
        let hashes;
        let mut pump;
        seeded_pump!(
            hashes,
            pump,
            pieces,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential
        );
        // Piece 0 arrives corrupt: rejected as data, burned, unpickable.
        let mut corrupt = pieces[0].clone();
        corrupt[7] ^= 0x20;
        let event = pump
            .handle_message(&Message::Piece {
                index: 0,
                begin: 0,
                block: Cow::Owned(corrupt),
            })
            .unwrap();
        assert_eq!(event, SessionEvent::PieceRejected { index: 0 });
        assert_eq!(pump.rejected(), [0]);
        assert_eq!(pump.verified_count(), 0);
        // Re-filing while burned is still the assembler's typed error.
        assert!(
            pump.handle_message(&serve(
                &pieces,
                &BlockPlan {
                    index: 0,
                    begin: 0,
                    length: 128
                }
            ))
            .is_err()
        );
        // The requeue loop reopens the rejected piece and reports what it
        // requeued, in rejection order.
        let requeued = pump.retry_rejected().unwrap();
        assert_eq!(requeued, vec![0]);
        assert_eq!(pump.rejected(), []);
        // A second requeue finds nothing: the drain is total.
        assert_eq!(pump.retry_rejected().unwrap(), []);
        // The piece is planned again and completes with the true bytes.
        let plan = pump.next_request().unwrap().unwrap();
        assert_eq!((plan.index, plan.begin, plan.length), (0, 0, 128));
        let event = pump.handle_message(&serve(&pieces, &plan)).unwrap();
        assert_eq!(
            event,
            SessionEvent::PieceVerified {
                index: 0,
                bytes: pieces[0].clone()
            }
        );
        // The untouched piece follows, and the download completes.
        let plan = pump.next_request().unwrap().unwrap();
        assert_eq!(plan.index, 1);
        pump.handle_message(&serve(&pieces, &plan)).unwrap();
        assert!(pump.is_complete());
        assert_eq!(pump.rejected(), []);
        assert_eq!(pump.verified_count(), 2);
    }

    #[test]
    fn pump_stops_requesting_while_choked_and_resumes_on_unchoke() {
        let pieces = vec![piece_bytes(0, 128), piece_bytes(1, 128)];
        let hashes;
        let mut pump;
        seeded_pump!(
            hashes,
            pump,
            pieces,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential
        );
        let plan = pump.next_request().unwrap().unwrap();
        let event = pump.handle_message(&Message::Choke).unwrap();
        assert_eq!(
            event,
            SessionEvent::PeerChoked {
                evicted: vec![plan]
            }
        );
        assert!(pump.pipeline().is_choked());
        assert!(pump.next_request().unwrap().is_none());
        let event = pump.handle_message(&Message::Unchoke).unwrap();
        assert_eq!(event, SessionEvent::PeerUnchoked);
        // The evicted block was never served, so it is re-requested.
        let again = pump.next_request().unwrap().unwrap();
        assert_eq!(again, plan);
    }

    #[test]
    fn pump_respects_the_depth_cap_and_cancels_free_slots() {
        // 20_000-byte pieces tile into two blocks: one full BLOCK_LEN, one
        // remainder — the classic multi-block layout.
        let pieces = vec![piece_bytes(0, 20_000), piece_bytes(1, 20_000)];
        let hashes;
        let mut pump;
        seeded_pump!(hashes, pump, pieces, 1, PieceStrategy::Sequential);
        let first = pump.next_request().unwrap().unwrap();
        assert_eq!((first.index, first.begin, first.length), (0, 0, BLOCK_LEN));
        // The 1-request cap is full.
        assert!(pump.next_request().unwrap().is_none());
        // Canceling frees the slot and speaks wire Cancel with layout lengths.
        let cancel = pump.cancel_request(0, 0).unwrap().unwrap();
        assert_eq!(
            cancel,
            Message::Cancel {
                index: 0,
                begin: 0,
                length: BLOCK_LEN
            }
        );
        assert!(pump.cancel_request(0, 0).unwrap().is_none());
        let again = pump.next_request().unwrap().unwrap();
        assert_eq!(again, first);
        // Serving the first block buffers the piece one block short.
        let event = pump.handle_message(&serve(&pieces, &again)).unwrap();
        assert_eq!(
            event,
            SessionEvent::BlockBuffered {
                index: 0,
                pending: 1
            }
        );
        // The next plan continues the piece, skipping the served block.
        let second = pump.next_request().unwrap().unwrap();
        assert_eq!((second.index, second.begin), (0, BLOCK_LEN));
    }

    #[test]
    fn pump_routes_control_messages_to_ignored() {
        let pieces = vec![piece_bytes(0, 128)];
        let hashes;
        let mut pump;
        seeded_pump!(
            hashes,
            pump,
            pieces,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential
        );
        for message in [
            Message::KeepAlive,
            Message::Interested,
            Message::NotInterested,
            Message::Port(6_881),
            Message::Request {
                index: 0,
                begin: 0,
                length: BLOCK_LEN,
            },
            Message::Unknown {
                id: 99,
                payload: Cow::Borrowed(&[1_u8, 2, 3]),
            },
        ] {
            let event = pump.handle_message(&message).unwrap();
            assert_eq!(event, SessionEvent::Ignored, "for {message:?}");
        }
        // Inbound cancels are bookkeeping events, not errors.
        let event = pump
            .handle_message(&Message::Cancel {
                index: 0,
                begin: 0,
                length: BLOCK_LEN,
            })
            .unwrap();
        assert_eq!(event, SessionEvent::PeerCancel { index: 0, begin: 0 });
    }

    #[test]
    fn pump_surfacing_data_for_a_verified_piece_is_a_typed_error() {
        let pieces = vec![piece_bytes(0, 128), piece_bytes(1, 128)];
        let hashes;
        let mut pump;
        seeded_pump!(
            hashes,
            pump,
            pieces,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential
        );
        let plan = pump.next_request().unwrap().unwrap();
        pump.handle_message(&serve(&pieces, &plan)).unwrap();
        assert!(pump.verified_field().has(0));
        // A hostile or broken peer sends the block again.
        let err = pump.handle_message(&serve(&pieces, &plan)).unwrap_err();
        assert!(err.to_string().contains("already assembled"));
    }

    #[test]
    fn pump_ordering_matches_the_piece_strategy() {
        let pieces = vec![
            piece_bytes(0, 128),
            piece_bytes(1, 128),
            piece_bytes(2, 128),
        ];
        let hashes = hashes_for(&pieces);
        let (count, length, total) = layout(&pieces);
        // Rarest-first: the peer holds pieces 0 and 1; a redundant Have on
        // piece 0 routes another count into it, so piece 1 is scarcest and
        // must win even though piece 0 has the lower index.
        let mut pump = SessionPump::new(
            &hashes,
            count,
            length,
            total,
            8,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::RarestFirst,
        )
        .unwrap();
        pump.handle_message(&Message::Bitfield(Cow::Borrowed(&[0b1100_0000])))
            .unwrap();
        pump.handle_message(&Message::Have(0)).unwrap();
        let plan = pump.next_request().unwrap().unwrap();
        assert_eq!(plan.index, 1);
        // Sequential: the same swarm state picks the lowest index instead.
        let mut pump = SessionPump::new(
            &hashes,
            count,
            length,
            total,
            8,
            MAX_PIPELINE_DEPTH,
            PieceStrategy::Sequential,
        )
        .unwrap();
        pump.handle_message(&Message::Bitfield(Cow::Borrowed(&[0b1100_0000])))
            .unwrap();
        pump.handle_message(&Message::Have(0)).unwrap();
        let plan = pump.next_request().unwrap().unwrap();
        assert_eq!(plan.index, 0);
    }
}
