//! # `session_stream` — the `tokio` I/O edge of the session (M1 remainder)
//!
//! The runtime-free [`SessionPump`] is the whole per-connection state machine:
//! it routes every wire [`Message`] through `assembler → verifier` and plans
//! the next outbound block under a pipeline cap. What it deliberately never
//! owned was the socket. This module is that socket edge, and *only* that
//! socket edge: a [`StreamSession`] owns one [`SessionPump`] plus an
//! [`tokio::io::AsyncRead`] half and an [`tokio::io::AsyncWrite`] half, and
//! drives them in a strict pump so `tokio` enters only at the I/O boundary.
//!
//! ## Plan fidelity
//!
//! `ARCHITECTURE.md` §9, the **M1 (remainder)** row, pins the acceptance
//! criterion for this module:
//!
//! > `tokio` enters *only* at the I/O edge; codec, bookkeeping, routing,
//! > verification, endgame, and the upload policy all stay runtime-free, so
//! > the reader is a thin pump over `handle_message` /
//! > `next_request(_endgame)` / `plan_upload`.
//!
//! That contract is enforced structurally: [`StreamSession`] holds a
//! [`SessionPump`] by value and never reaches into its internals; every
//! inbound frame goes through [`SessionPump::handle_message`], every
//! outbound plan through [`SessionPump::next_request`] (or the endgame twin
//! when a board is shared), and every inbound `Request` through
//! [`SessionPump::plan_upload`]. The codec stays the runtime-free
//! [`crate::peer_wire::next_message`] / [`crate::peer_wire::Handshake`], and
//! [`Message::into_owned`] is the single seam that crosses an await point.
//!
//! ## House rules, unchanged from every shipped edge
//!
//! - Adversarial input is *data*: a truncated handshake or a malformed frame
//!   is a typed [`Error::Peer`]; a hostile length prefix is rejected before
//!   any allocation. Nothing panics.
//! - Byte strings stay borrowed until the await point: the codec parses out
//!   of the read buffer, and only the one decoded message is [`owned`] as it
//!   crosses into the pump.
//! - The pump holds no bytes: verified piece bytes surface on
//!   [`StepEvent::PieceVerified`] for the caller's storage (M3), and the
//!   upload side slices served bytes out of a caller-supplied closure, so
//!   the session never buffers a piece it cannot immediately hand off.
//! - `tracing` spans ride the seams ([`StreamSession::step`],
//!   [`StreamSession::serve`]); they are no-ops until a subscriber is
//!   installed, so embedders pay nothing for them.
//!
//! [`owned`]: crate::peer_wire::Message::into_owned

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::downloader::{BLOCK_LEN, BlockPlan, PieceStrategy};
use crate::endgame::EndgameBoard;
use crate::error::{Error, Result};
use crate::peer_wire::{HANDSHAKE_LEN, Handshake, Message};
use crate::session::{ServeDecision, SessionEvent, SessionPump};
use crate::storage::Storage;

/// The maximum read-buffer growth before the stream reader refuses a peer: a
/// single frame cannot exceed [`crate::peer_wire::MAX_MESSAGE_LEN`], and the
/// handshake is fixed at [`HANDSHAKE_LEN`], so a buffer larger than that plus
/// the length prefix is a peer that never frames a complete message.
const MAX_READ_BUFFER: usize = crate::peer_wire::MAX_MESSAGE_LEN + 4;

/// What one pump step surfaced to the caller.
///
/// The session is a *thin* pump: it owns the socket edge and the
/// [`SessionPump`], but it hands every decision the pump makes straight to
/// the caller — verified bytes for storage, rejected indexes for the
/// re-request loop, control events for the swarm layer. `Idle` means one
/// full read→route→drain cycle produced nothing the caller must act on
/// (a `KeepAlive`, an `Interested`, a duplicate `Have`): the session is
/// ready for the next [`StreamSession::step`].
#[derive(Debug)]
pub enum StepEvent {
    /// One full pump cycle: a frame was read, routed through
    /// [`SessionPump::handle_message`], and any outbound plan the pump
    /// produced was drained and written back. The wrapped event is the
    /// pump's verdict; `outbound` is the block plan that was written, if
    /// any (the pump may plan nothing while choked, full, or complete).
    Routed {
        /// The pump's verdict for the routed frame.
        event: SessionEvent,
        /// The outbound plan written this step, if the pump planned one.
        outbound: Option<BlockPlan>,
    },
    /// The peer sent an inbound `Request` the upload policy judged. The
    /// session never holds piece bytes, so when the verdict is a
    /// [`ServeDecision::Serve`] the caller's storage (the `lookup` closure
    /// or the [`crate::storage::Storage`] backend) supplied the bytes and
    /// the served slice was written before this event surfaced. `index`
    /// names the piece the request was about.
    UploadJudged {
        /// The piece the judged request named.
        index: u32,
        /// Whether the verdict served the block or refused it as data.
        served: bool,
    },
    /// The pump drained no new frame and no outbound plan — the peer went
    /// quiet (only `KeepAlive`/control), or the torrent is complete. The
    /// next [`StreamSession::step`] reads again.
    Idle,
}

/// The layout + pump parameters for a [`StreamSession`]: the borrowed piece
/// digests and the torrent shape, plus the pump's buffering and pipeline
/// caps and its candidate strategy.
///
/// One struct so [`StreamSession::handshake`] stays a small, named-argument
/// call instead of an 11-positional one — and so the caller configures the
/// torrent once and reuses the same `StreamConfig` across every connection
/// in the swarm.
#[derive(Debug, Clone)]
pub struct StreamConfig<'a> {
    /// The metainfo's concatenated 20-byte SHA-1 digests, borrowed for the
    /// pump's verifier exactly like [`SessionPump::new`].
    pub pieces: &'a [u8],
    /// How many pieces the torrent is laid out for.
    pub piece_count: u32,
    /// The byte length of every piece but the last.
    pub piece_length: u32,
    /// The torrent's total byte length (the last piece is the remainder).
    pub total_length: u64,
    /// The cap on partial pieces buffered in the assembler (see
    /// [`crate::downloader::BlockAssembler`]).
    pub max_buffered: u32,
    /// The per-connection in-flight request cap (see
    /// [`crate::session::MAX_PIPELINE_DEPTH`]).
    pub pipeline_depth: u32,
    /// How the pump orders candidate pieces (sequential or rarest-first).
    pub strategy: PieceStrategy,
}

impl<'a> StreamConfig<'a> {
    /// A `StreamConfig` with the borrowed digests and the torrent's full
    /// shape, defaulting the pump caps to `max_buffered = 8` and
    /// `pipeline_depth = 250` (BEP 3 practice) and the strategy to
    /// [`PieceStrategy::RarestFirst`].
    #[must_use]
    pub fn new(pieces: &'a [u8], piece_count: u32, piece_length: u32, total_length: u64) -> Self {
        Self {
            pieces,
            piece_count,
            piece_length,
            total_length,
            max_buffered: 8,
            pipeline_depth: crate::session::MAX_PIPELINE_DEPTH,
            strategy: PieceStrategy::RarestFirst,
        }
    }

    /// Sets the partial-piece buffering cap.
    #[must_use]
    pub const fn with_max_buffered(mut self, max_buffered: u32) -> Self {
        self.max_buffered = max_buffered;
        self
    }

    /// Sets the per-connection in-flight request cap.
    #[must_use]
    pub const fn with_pipeline_depth(mut self, pipeline_depth: u32) -> Self {
        self.pipeline_depth = pipeline_depth;
        self
    }

    /// Sets the candidate ordering strategy.
    #[must_use]
    pub const fn with_strategy(mut self, strategy: PieceStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

/// The `tokio` I/O edge of one peer connection: a thin pump over the
/// runtime-free [`SessionPump`].
///
/// Construct it with [`StreamSession::handshake`] once the BEP 10 exchange
/// is done, then drive [`StreamSession::step`] in a loop. The session owns
/// the read and write halves independently (so a `TcpStream` split with
/// [`tokio::net::TcpStream::split`] drops straight in), and it borrows the
/// metainfo's piece hashes for the pump's verifier — exactly like the
/// standalone [`SessionPump`].
///
/// ```no_run
/// # #[cfg(feature = "stream")] {
/// # use magnetite_core::downloader::PieceStrategy;
/// # use magnetite_core::infohash::InfoHash;
/// # use magnetite_core::session_stream::{StreamConfig, StreamSession};
/// # async fn demo(
/// #     hashes: &'static [u8], reader: tokio::net::tcp::OwnedReadHalf,
/// #     writer: tokio::net::tcp::OwnedWriteHalf,
/// # ) -> Result<(), magnetite_core::Error> {
/// let info_hash = InfoHash::from_bytes([0xAB; 20]);
/// let our_id = *b"-qB4550-magnetite-x!";
/// let config = StreamConfig::new(hashes, 2, 20_000, 40_000)
///     .with_strategy(PieceStrategy::RarestFirst);
/// let mut session =
///     StreamSession::handshake(reader, writer, info_hash, our_id, &config).await?;
/// while !session.is_complete() {
///     session.step(|_index, _begin, _length| None).await?;
/// }
/// # Ok(())
/// # }
/// # }
/// ```
pub struct StreamSession<'a, R, W> {
    reader: R,
    writer: W,
    pump: SessionPump<'a>,
    read_buf: Vec<u8>,
    /// The remote handshake, kept for the caller's extension negotiation
    /// (DHT bit, Fast Extension, BEP 10 extensions).
    remote: Handshake,
}

impl<'a, R, W> StreamSession<'a, R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Performs the BEP 10 handshake exchange and binds the runtime-free
    /// [`SessionPump`] to the connection.
    ///
    /// Writes our 68-byte handshake, reads the peer's 68-byte handshake
    /// (incrementally, so stream chunks of any size are fine), and
    /// constructs the pump against the metainfo's borrowed piece digests.
    /// The remote's reserved bytes are kept on the session for the caller's
    /// extension negotiation.
    ///
    /// # Errors
    /// Returns [`Error::Peer`] when the peer's handshake is malformed, and
    /// [`Error::Io`] when the read or write fails. Returns
    /// [`Error::Download`] when the metainfo layout is inconsistent (see
    /// [`SessionPump::new`]).
    pub async fn handshake(
        mut reader: R,
        mut writer: W,
        info_hash: crate::infohash::InfoHash,
        our_peer_id: [u8; 20],
        config: &StreamConfig<'a>,
    ) -> Result<Self> {
        let span = tracing::debug_span!("session_stream::handshake");
        let _enter = span.enter();
        let ours = Handshake::new(info_hash, our_peer_id);
        writer.write_all(&ours.encode()).await?;
        // Read the full 68-byte handshake incrementally: a stream chunk may
        // carry fewer bytes than the handshake, and the rest arrives in the
        // next read.
        let mut buf = vec![0_u8; HANDSHAKE_LEN];
        reader.read_exact(&mut buf).await?;
        let remote = Handshake::parse(&buf)?;
        tracing::debug!(?remote.info_hash, "peer handshake complete");
        let pump = SessionPump::new(
            config.pieces,
            config.piece_count,
            config.piece_length,
            config.total_length,
            config.max_buffered,
            config.pipeline_depth,
            config.strategy,
        )?;
        Ok(Self {
            reader,
            writer,
            pump,
            read_buf: Vec::new(),
            remote,
        })
    }

    /// Drives one full pump cycle: read one frame, route it through
    /// [`SessionPump::handle_message`], judge any inbound `Request` via
    /// [`SessionPump::plan_upload`] and serve its bytes from the caller's
    /// storage, then drain one outbound plan via [`SessionPump::next_request`]
    /// and write it.
    ///
    /// `lookup` is the caller's storage seam: when the routed frame is an
    /// inbound `Request` the upload policy judges as
    /// [`ServeDecision::Serve`], the session calls `lookup(index, begin,
    /// length)` to fetch the verified bytes and writes the `Piece` reply
    /// before returning. Returning `None` from `lookup` refuses the block
    /// (the policy already cleared it; the bytes simply are not on hand
    /// yet). The pump holds no piece bytes, so the lookup is the only way
    /// served bytes reach the wire. Callers with a real backend implement
    /// [`crate::storage::Storage`] and use [`StreamSession::step_with_storage`]
    /// instead.
    ///
    /// # Errors
    /// Returns [`Error::Peer`] on a malformed frame, [`Error::Download`]
    /// when the pump rejects a layout violation, and [`Error::Io`] on a
    /// read or write failure.
    pub async fn step<F>(&mut self, lookup: F) -> Result<StepEvent>
    where
        F: FnOnce(u32, u32, u32) -> Option<Vec<u8>>,
    {
        let span = tracing::debug_span!("session_stream::step");
        let _enter = span.enter();
        let (event, request) = self.read_and_route().await?;
        // An inbound `Request` is judged by `plan_upload` and, when it serves,
        // the bytes come straight out of the caller's storage via `lookup`.
        let upload = match request {
            Some((index, begin, length)) => {
                let served = self.serve_lookup(index, begin, length, lookup).await?;
                Some((index, served))
            }
            None => None,
        };
        let outbound = self.drain_outbound().await?;
        if let Some((index, served)) = upload {
            return Ok(StepEvent::UploadJudged { index, served });
        }
        match event {
            SessionEvent::Ignored if outbound.is_none() => Ok(StepEvent::Idle),
            event => Ok(StepEvent::Routed { event, outbound }),
        }
    }

    /// Drives one full pump cycle with a [`Storage`] backend instead of a
    /// bare `lookup` closure: identical to [`StreamSession::step`], except
    /// a served inbound `Request` fetches its bytes through
    /// [`Storage::read_block`] — the M3 storage seam the upload policy was
    /// specified against (`ARCHITECTURE.md` §4.8: "the pump holds no bytes;
    /// storage stays M3").
    ///
    /// A piece the backend does not hold yet reads as `Ok(None)` and the
    /// block is refused as data, exactly like a closure returning `None`;
    /// a slice that violates the layout is a typed [`Error::Download`].
    ///
    /// # Errors
    /// Returns [`Error::Peer`] on a malformed frame, [`Error::Download`]
    /// when the pump or the backend rejects a layout violation, and
    /// [`Error::Io`] on a read or write failure.
    pub async fn step_with_storage(&mut self, storage: &mut dyn Storage) -> Result<StepEvent> {
        let span = tracing::debug_span!("session_stream::step_with_storage");
        let _enter = span.enter();
        let (event, request) = self.read_and_route().await?;
        let upload = match request {
            Some((index, begin, length)) => {
                let served = self.serve_storage(index, begin, length, storage).await?;
                Some((index, served))
            }
            None => None,
        };
        let outbound = self.drain_outbound().await?;
        if let Some((index, served)) = upload {
            return Ok(StepEvent::UploadJudged { index, served });
        }
        match event {
            SessionEvent::Ignored if outbound.is_none() => Ok(StepEvent::Idle),
            event => Ok(StepEvent::Routed { event, outbound }),
        }
    }

    /// Drives one pump cycle against the swarm's shared [`EndgameBoard`]:
    /// like [`StreamSession::step`] but the outbound plan comes from
    /// [`SessionPump::next_request_endgame`], and outcomes are fed back to
    /// the board the way the runtime-free twin documents.
    ///
    /// `lookup` is the same storage seam as [`StreamSession::step`]. The
    /// caller feeds block/piece outcomes back to the board itself; this
    /// edge only plans, routes, and writes.
    ///
    /// # Errors
    /// Returns [`Error::Peer`] on a malformed frame, [`Error::Download`]
    /// when the pump or the board rejects a layout violation, and
    /// [`Error::Io`] on a read or write failure.
    pub async fn step_endgame<F>(
        &mut self,
        board: &mut EndgameBoard,
        lookup: F,
    ) -> Result<StepEvent>
    where
        F: FnOnce(u32, u32, u32) -> Option<Vec<u8>>,
    {
        let span = tracing::debug_span!("session_stream::step_endgame");
        let _enter = span.enter();
        let (event, request) = self.read_and_route().await?;
        let upload = match request {
            Some((index, begin, length)) => {
                let served = self.serve_lookup(index, begin, length, lookup).await?;
                Some((index, served))
            }
            None => None,
        };
        let outbound = self.drain_outbound_endgame(board).await?;
        if let Some((index, served)) = upload {
            return Ok(StepEvent::UploadJudged { index, served });
        }
        match event {
            SessionEvent::Ignored if outbound.is_none() => Ok(StepEvent::Idle),
            event => Ok(StepEvent::Routed { event, outbound }),
        }
    }

    /// Drives one pump cycle against the swarm's shared [`EndgameBoard`]
    /// with a [`Storage`] backend: the endgame twin of
    /// [`StreamSession::step_with_storage`].
    ///
    /// # Errors
    /// Same contract as [`StreamSession::step_endgame`], plus the
    /// backend's own typed failures.
    pub async fn step_endgame_with_storage(
        &mut self,
        board: &mut EndgameBoard,
        storage: &mut dyn Storage,
    ) -> Result<StepEvent> {
        let span = tracing::debug_span!("session_stream::step_endgame_with_storage");
        let _enter = span.enter();
        let (event, request) = self.read_and_route().await?;
        let upload = match request {
            Some((index, begin, length)) => {
                let served = self.serve_storage(index, begin, length, storage).await?;
                Some((index, served))
            }
            None => None,
        };
        let outbound = self.drain_outbound_endgame(board).await?;
        if let Some((index, served)) = upload {
            return Ok(StepEvent::UploadJudged { index, served });
        }
        match event {
            SessionEvent::Ignored if outbound.is_none() => Ok(StepEvent::Idle),
            event => Ok(StepEvent::Routed { event, outbound }),
        }
    }

    /// The remote handshake, for extension negotiation (DHT bit, Fast
    /// Extension, BEP 10 extensions).
    #[must_use]
    pub const fn remote_handshake(&self) -> Handshake {
        self.remote
    }

    /// The runtime-free pump this edge drives. Exposed so the caller can
    /// run the swarm-aware queries (`verified_count`, `is_complete`,
    /// `rejected`, `retry_rejected`) the pump already documents.
    #[must_use]
    pub const fn pump(&self) -> &SessionPump<'a> {
        &self.pump
    }

    /// A mutable handle to the pump, for `retry_rejected` and the swarm
    /// queries that mutate state.
    pub const fn pump_mut(&mut self) -> &mut SessionPump<'a> {
        &mut self.pump
    }

    /// Whether every piece has passed verification.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.pump.is_complete()
    }

    /// Reads exactly enough bytes for one complete frame and routes it
    /// through the pump. Returns the pump's verdict and, when the routed
    /// frame was an inbound `Request`, the `(index, begin, length)` the
    /// upload policy will judge — the pump routes `Request` to `Ignored`,
    /// so the raw params are the only way `step` can serve it inline.
    async fn read_and_route(&mut self) -> Result<(SessionEvent, Option<(u32, u32, u32)>)> {
        loop {
            if let Some((message, consumed)) = crate::peer_wire::next_message(&self.read_buf)? {
                // The message borrows from `read_buf`; detach it before we
                // drop the consumed prefix (the await point the codec's
                // `into_owned` seam exists for).
                let request = match &message {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => Some((*index, *begin, *length)),
                    _ => None,
                };
                let message = message.into_owned();
                self.read_buf.drain(..consumed);
                let event = self.pump.handle_message(&message)?;
                return Ok((event, request));
            }
            // No complete frame yet: read more, bounded by the wire cap.
            if self.read_buf.len() >= MAX_READ_BUFFER {
                return Err(Error::Peer(format!(
                    "read buffer reached {} bytes without a complete frame",
                    self.read_buf.len()
                )));
            }
            // Heap-allocated read chunk: a stack array held across the await
            // would bloat the future past the pedantic `large_futures` cap
            // (BLOCK_LEN is 16 KiB), and a small heap allocation is cheaper
            // than re-sizing the state machine.
            let mut chunk = vec![0_u8; BLOCK_LEN as usize];
            let n = self.reader.read(&mut chunk).await?;
            if n == 0 {
                return Err(Error::Peer(format!(
                    "peer closed the connection with {} unframed bytes",
                    self.read_buf.len()
                )));
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Drains one outbound plan from [`SessionPump::next_request`] and
    /// writes its wire form. Returns the plan that was written, if any.
    async fn drain_outbound(&mut self) -> Result<Option<BlockPlan>> {
        let Some(plan) = self.pump.next_request()? else {
            return Ok(None);
        };
        self.write_plan(plan).await?;
        Ok(Some(plan))
    }

    /// Drains one outbound plan from [`SessionPump::next_request_endgame`]
    /// and writes its wire form. Returns the plan that was written, if any.
    async fn drain_outbound_endgame(
        &mut self,
        board: &mut EndgameBoard,
    ) -> Result<Option<BlockPlan>> {
        let Some(plan) = self.pump.next_request_endgame(board)? else {
            return Ok(None);
        };
        self.write_plan(plan).await?;
        Ok(Some(plan))
    }

    /// Encodes a `Request` plan and writes it.
    async fn write_plan(&mut self, plan: BlockPlan) -> Result<()> {
        let mut frame = Vec::with_capacity(17);
        plan.to_message().encode_into(&mut frame);
        self.writer.write_all(&frame).await?;
        tracing::debug!(
            index = plan.index,
            begin = plan.begin,
            length = plan.length,
            "outbound request written"
        );
        Ok(())
    }

    /// The shared upload half of the I/O edge: judges an inbound `Request`
    /// (`index`, `begin`, `length`) against [`SessionPump::plan_upload`], and
    /// if it serves, fetches the verified bytes via `lookup` and writes the
    /// `Piece` reply. Returns whether a block was served.
    ///
    /// `serve` (the public entry point) and `step`/`step_endgame` (the inline
    /// pump cycle) both go through here, so the upload policy is judged
    /// exactly once per inbound `Request` no matter which path delivered it.
    async fn serve_lookup<F>(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
        lookup: F,
    ) -> Result<bool>
    where
        F: FnOnce(u32, u32, u32) -> Option<Vec<u8>>,
    {
        match self.pump.plan_upload(index, begin, length)? {
            ServeDecision::Serve { plan } => {
                let bytes = lookup(index, begin, length);
                self.write_served(index, begin, plan, bytes).await
            }
            ServeDecision::PieceNotVerified { index } => {
                tracing::debug!(index, "upload refused: piece not verified");
                Ok(false)
            }
        }
    }

    /// The [`Storage`]-backed upload half: identical judgment, but the
    /// served bytes come out of a backend via [`Storage::read_block`] — a
    /// piece the backend does not hold reads as `Ok(None)` and the block is
    /// refused as data, while a layout-violating slice surfaces the
    /// backend's typed error.
    ///
    /// `serve_with_storage` and the `*_with_storage` pump cycles all go
    /// through here.
    async fn serve_storage(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
        storage: &mut dyn Storage,
    ) -> Result<bool> {
        match self.pump.plan_upload(index, begin, length)? {
            ServeDecision::Serve { plan } => {
                let bytes = storage.read_block(index, begin, plan.length)?;
                self.write_served(index, begin, plan, bytes).await
            }
            ServeDecision::PieceNotVerified { index } => {
                tracing::debug!(index, "upload refused: piece not verified");
                Ok(false)
            }
        }
    }

    /// Validates the fetched bytes against the served plan and writes the
    /// `Piece` reply. `None` bytes refuse the block as data.
    async fn write_served(
        &mut self,
        index: u32,
        begin: u32,
        plan: BlockPlan,
        bytes: Option<Vec<u8>>,
    ) -> Result<bool> {
        let Some(bytes) = bytes else {
            tracing::debug!(index, "upload refused: caller has no bytes yet");
            return Ok(false);
        };
        if bytes.len() != usize::try_from(plan.length).unwrap_or(usize::MAX) {
            return Err(Error::Download(format!(
                "storage returned {} bytes for piece {index} block at {begin}, the layout plans {}",
                bytes.len(),
                plan.length
            )));
        }
        let mut frame = Vec::with_capacity(9 + bytes.len());
        Message::Piece {
            index,
            begin,
            block: std::borrow::Cow::Owned(bytes),
        }
        .encode_into(&mut frame);
        self.writer.write_all(&frame).await?;
        tracing::debug!(index, begin, length = plan.length, "outbound piece served");
        Ok(true)
    }

    /// Judges an inbound `Request` (`index`, `begin`, `length`) against the
    /// upload policy, and if it serves, fetches the verified bytes via
    /// `lookup` and writes the `Piece` reply.
    ///
    /// This is the explicit upload entry point: callers that drive the
    /// download loop with [`StreamSession::step`] get inbound `Request`s
    /// served inline (via [`SessionPump::plan_upload`]), but a caller that
    /// wants to serve a request outside the pump cycle — for example, a
    /// seeder that only answers, never downloads — calls `serve` directly.
    /// [`SessionPump::plan_upload`] names the exact [`BlockPlan`] to serve;
    /// the session slices the bytes out of the caller's storage and writes
    /// them. A refusal ([`ServeDecision::PieceNotVerified`]) is data, not an
    /// error, so the session simply does not write.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the request does not match the
    /// layout, and [`Error::Io`] on a write failure.
    pub async fn serve<F>(&mut self, index: u32, begin: u32, length: u32, lookup: F) -> Result<bool>
    where
        F: FnOnce(u32, u32, u32) -> Option<Vec<u8>>,
    {
        let span = tracing::debug_span!("session_stream::serve");
        let _enter = span.enter();
        self.serve_lookup(index, begin, length, lookup).await
    }

    /// Judges an inbound `Request` (`index`, `begin`, `length`) against the
    /// upload policy and, if it serves, fetches the bytes through a
    /// [`Storage`] backend and writes the `Piece` reply.
    ///
    /// This is the [`Storage`]-backed twin of [`StreamSession::serve`]: a
    /// seeder holding verified pieces in a real backend calls this with
    /// `&mut dyn Storage` instead of wiring a closure around it. A refusal
    /// ([`ServeDecision::PieceNotVerified`] or a piece the backend does not
    /// hold yet) is data, not an error — nothing is written.
    ///
    /// # Errors
    /// Returns [`Error::Download`] when the request does not match the
    /// layout or the backend rejects the slice, and [`Error::Io`] on a
    /// write failure.
    pub async fn serve_with_storage(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
        storage: &mut dyn Storage,
    ) -> Result<bool> {
        let span = tracing::debug_span!("session_stream::serve_with_storage");
        let _enter = span.enter();
        self.serve_storage(index, begin, length, storage).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::PieceField;
    use crate::infohash::InfoHash;
    use sha1::{Digest, Sha1};

    /// The two-piece, 40_000-byte layout every shipped session spec uses:
    /// pinning it here keeps the stream specs on the same fixture the
    /// runtime-free pump is proven against.
    const PIECE_COUNT: u32 = 2;
    const PIECE_LENGTH: u32 = 20_000;
    const TOTAL: u64 = 40_000;

    /// Deterministic payload: offset `o` of piece `p` is `(7p + o) % 251`,
    /// the same pattern the pump and verifier specs use.
    fn piece_bytes(piece: u32, length: u32) -> Vec<u8> {
        (0..length)
            .map(|offset| {
                u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251).unwrap_or(0)
            })
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

    /// The flattened payload, so the fake peer can slice blocks out of one
    /// contiguous buffer (matching how a real storage layer would).
    fn flat(pieces: &[Vec<u8>]) -> Vec<u8> {
        pieces.iter().flatten().copied().collect()
    }

    /// A fake peer driving the writer side of a duplex pair: it sends the
    /// handshake and bitfield, then answers every `Request` the session
    /// writes with the true bytes. Owned data only — it crosses the spawn.
    struct PeerDriver {
        tx: tokio::io::DuplexStream,
        rx: tokio::io::DuplexStream,
    }

    impl PeerDriver {
        /// Sends the peer's handshake and a full bitfield, then enters its
        /// answer loop: for every `Request` the session writes, it serves
        /// the matching block from `payload`.
        async fn drive(&mut self, info_hash: InfoHash, payload: Vec<u8>, expected_requests: u32) {
            // Read and discard the session's handshake first: the session
            // writes its 68-byte handshake as the first thing on the wire,
            // before any Request frames.
            let mut their_hs = [0_u8; HANDSHAKE_LEN];
            self.rx.read_exact(&mut their_hs).await.unwrap();

            let hs = Handshake::new(info_hash, *b"-qB4550-magnetite-y!");
            self.tx.write_all(&hs.encode()).await.unwrap();
            let mut wire = PieceField::new(PIECE_COUNT).unwrap();
            for index in 0..PIECE_COUNT {
                wire.set_have(index).unwrap();
            }
            let mut frame = Vec::new();
            Message::Bitfield(std::borrow::Cow::Owned(wire.to_bitfield())).encode_into(&mut frame);
            self.tx.write_all(&frame).await.unwrap();

            let mut buf = [0_u8; 17];
            for _ in 0..expected_requests {
                self.rx.read_exact(&mut buf).await.unwrap();
                let plan = match crate::peer_wire::parse_message(&buf).unwrap() {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => BlockPlan {
                        index,
                        begin,
                        length,
                    },
                    other => panic!("expected a Request, got {other:?}"),
                };
                let start = plan.index as usize * PIECE_LENGTH as usize + plan.begin as usize;
                let block = payload[start..start + plan.length as usize].to_vec();
                let mut frame = Vec::new();
                Message::Piece {
                    index: plan.index,
                    begin: plan.begin,
                    block: std::borrow::Cow::Owned(block),
                }
                .encode_into(&mut frame);
                self.tx.write_all(&frame).await.unwrap();
            }
        }

        /// Reads one outbound `Piece` frame the session served.
        async fn recv_piece(&mut self, length: usize) -> Message<'static> {
            let mut buf = vec![0_u8; 4 + 1 + 8 + length];
            self.rx.read_exact(&mut buf).await.unwrap();
            crate::peer_wire::parse_message(&buf).unwrap().into_owned()
        }
    }

    /// Resumes the driver so it answers the remaining `Request`s and then
    /// returns. Call this after the session steps through to completion.
    async fn resume_driver(driver_task: tokio::task::JoinHandle<PeerDriver>) {
        driver_task.await.unwrap();
    }

    /// Plan-fidelity pin: the stream session is a *thin pump* over the
    /// runtime-free `SessionPump`. This test pins the plan's M1 (remainder)
    /// acceptance criterion verbatim — `ARCHITECTURE.md` §9:
    ///
    /// > `tokio` enters *only* at the I/O edge; codec, bookkeeping, routing,
    /// > verification, endgame, and the upload policy all stay runtime-free,
    /// > so the reader is a thin pump over `handle_message` /
    /// > `next_request(_endgame)` / `plan_upload`.
    ///
    /// A happy-path download through the async edge must verify the same
    /// pieces the runtime-free pump verifies in `session.rs`, and the peer
    /// must see exactly the `Request` frames the pump plans — proof the
    /// edge pumps and never invents its own protocol.
    #[tokio::test]
    async fn stream_session_is_a_thin_pump_over_handle_message_and_next_request() {
        // `leak` lifts the hashes to `'static` so the session's borrowed
        // verifier outlives the test — the same lifetime trick the
        // runtime-free pump specs rely on via `seeded_pump!`.
        let pieces = vec![piece_bytes(0, PIECE_LENGTH), piece_bytes(1, PIECE_LENGTH)];
        let hashes: &'static [u8] = Box::leak(hashes_for(&pieces).into_boxed_slice());
        let payload = flat(&pieces);
        let info_hash = InfoHash::from_bytes([0xAB; 20]);

        let (peer_tx, session_rx) = tokio::io::duplex(8 * 1024);
        let (session_tx, peer_rx) = tokio::io::duplex(8 * 1024);
        let mut driver = PeerDriver {
            tx: peer_tx,
            rx: peer_rx,
        };
        let driver_task = tokio::spawn(async move {
            driver.drive(info_hash, payload, 4).await;
            driver
        });

        let config = StreamConfig::new(hashes, PIECE_COUNT, PIECE_LENGTH, TOTAL)
            .with_strategy(PieceStrategy::Sequential);
        let mut session = StreamSession::handshake(
            session_rx,
            session_tx,
            info_hash,
            *b"-qB4550-magnetite-x!",
            &config,
        )
        .await
        .expect("handshake completes against the driver");

        // The pump plans four blocks (two per piece). Each `step` reads one
        // frame and drains one outbound plan: the first frame is the peer's
        // bitfield, the next four are the served blocks, so five steps verify
        // both pieces — the same end state as the runtime-free pump in
        // `session.rs`.
        let mut verified = 0_u32;
        for _ in 0..5 {
            let step = session
                .step(|_, _, _| None)
                .await
                .expect("the step pumps the routed frame");
            if let StepEvent::Routed {
                event: SessionEvent::PieceVerified { index, .. },
                ..
            } = step
            {
                assert!(index < PIECE_COUNT);
                verified += 1;
            }
        }
        assert_eq!(verified, PIECE_COUNT);
        assert!(session.is_complete());
        resume_driver(driver_task).await;
    }

    /// The handshake edge rejects a peer that closes mid-handshake as data,
    /// never a panic.
    #[tokio::test]
    async fn handshake_rejects_a_short_peer_as_data() {
        // Pair 1 carries the session's read half; dropping its writer closes
        // the pipe so `read_exact` sees EOF. Pair 2 carries the session's
        // write half (our handshake) — its buffer must hold the 68-byte
        // handshake or the write blocks before the read even gets to fail.
        let (peer_tx, session_rx) = tokio::io::duplex(8 * 1024);
        let (_session_tx, peer_rx) = tokio::io::duplex(8 * 1024);
        // Drop the peer's writer without sending anything.
        drop(peer_tx);
        let result = StreamSession::handshake(
            session_rx,
            peer_rx,
            InfoHash::from_bytes([0xAB; 20]),
            *b"-qB4550-magnetite-x!",
            &StreamConfig::new(&[0_u8; 20], 1, BLOCK_LEN, u64::from(BLOCK_LEN)),
        )
        .await;
        assert!(result.is_err());
    }

    /// The upload edge serves verified bytes from the caller's storage:
    /// `plan_upload` names the block, the `lookup` closure supplies the
    /// bytes, and the peer receives the `Piece`. An unverified piece is
    /// refused as data — nothing is written.
    #[tokio::test]
    async fn serve_writes_verified_bytes_and_refuses_unverified_as_data() {
        let pieces = vec![piece_bytes(0, PIECE_LENGTH), piece_bytes(1, PIECE_LENGTH)];
        let hashes: &'static [u8] = Box::leak(hashes_for(&pieces).into_boxed_slice());
        let payload = flat(&pieces);
        let info_hash = InfoHash::from_bytes([0xAB; 20]);

        let (peer_tx, session_rx) = tokio::io::duplex(8 * 1024);
        let (session_tx, peer_rx) = tokio::io::duplex(8 * 1024);
        let mut driver = PeerDriver {
            tx: peer_tx,
            rx: peer_rx,
        };
        // The driver answers the four download requests, then reads back the
        // one `Piece` the session serves out of the caller's storage.
        let driver_task = tokio::spawn(async move {
            driver.drive(info_hash, payload, 4).await;
            let served = driver.recv_piece(BLOCK_LEN as usize).await;
            (driver, served)
        });

        let config = StreamConfig::new(hashes, PIECE_COUNT, PIECE_LENGTH, TOTAL)
            .with_strategy(PieceStrategy::Sequential);
        let mut session = StreamSession::handshake(
            session_rx,
            session_tx,
            info_hash,
            *b"-qB4550-magnetite-x!",
            &config,
        )
        .await
        .expect("handshake completes against the driver");

        // Nothing verified yet: the upload is refused as data.
        let served = session
            .serve(0, 0, BLOCK_LEN, |_, _, _| None)
            .await
            .expect("a refusal is data, not an error");
        assert!(!served);

        // Complete the download so piece 0 is verified (five steps: the
        // bitfield plus four served blocks).
        for _ in 0..5 {
            session
                .step(|_, _, _| None)
                .await
                .expect("the step pumps the routed frame");
        }
        assert!(session.is_complete());

        // Serve piece 0's first block out of the caller's storage: the
        // `lookup` closure receives the (index, begin, length) the upload
        // policy judged, so it slices exactly the planned block out of the
        // caller's verified piece bytes.
        let storage = pieces[0].clone();
        let served = session
            .serve(0, 0, BLOCK_LEN, move |_, begin, length| {
                let start = begin as usize;
                Some(storage[start..start + length as usize].to_vec())
            })
            .await
            .expect("the served block matches the layout");
        assert!(served);

        // The peer received the Piece.
        let (_, msg) = driver_task.await.unwrap();
        assert!(matches!(
            msg,
            Message::Piece {
                index: 0,
                begin: 0,
                ..
            }
        ));
    }
}
