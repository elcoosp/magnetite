//! uTP (BEP 29): the micro transport protocol's wire codec and connection
//! bookkeeping, runtime-free.
//!
//! `ARCHITECTURE.md` §9 pins the **M2** row this module lands the uTP leg
//! of:
//!
//! > **M2** | DHT (BEP 5), PEX, uTP (BEP 29) | `magnetite-dht` crate;
//! > session state machine behind `&self` + internal tasks.
//!
//! uTP carries `BitTorrent` peer traffic over UDP, so its 20-byte header and
//! packet types are the transport seam the peer wire session eventually
//! rides. This module delivers the runtime-free half the rest of the stack
//! already expects: the fixed-header codec, the extension chain (including
//! BEP 34 selective acks), and a [`Connection`] state machine that tracks
//! sequence and ack bookkeeping with wraparound, paces sends by the peer's
//! advertised window and an additive congestion window, buffers bounded
//! out-of-order arrivals until the gap fills, and answers "what timed
//! out?" from caller-supplied timestamps — no clocks, no executor, no
//! I/O. The UDP socket edge that drives it stays future work, exactly
//! like every other groundwork edge in this crate.

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};

use crate::error::{Error, Result};

/// The fixed uTP header length (BEP 29).
pub const HEADER_LEN: usize = 20;

/// The only protocol version on the wire today.
pub const VERSION: u8 = 1;

/// Default maximum segment size for outbound data: 1500-byte IP MTU, minus
/// 20 IP bytes, minus 8 UDP bytes, minus the 20-byte uTP header.
pub const DEFAULT_MSS: usize = 1452;

/// The initial retransmission timeout in microseconds, doubled per
/// consecutive timeout pass (LEDBAT/uTP practice).
pub const DEFAULT_RTO_US: u32 = 500_000;

/// How many out-of-order packets the receive backlog may hold before
/// further future packets count as stale (bounded memory, house rule).
pub const MAX_BACKLOG_PACKETS: usize = 256;

/// The sequence-number span a receiver accepts ahead of its in-order
/// position: a datagram farther ahead answers as [`Incoming::Stale`].
pub const MAX_RECEIVE_WINDOW: u16 = 1024;

/// The receive window every outbound packet advertises, in bytes — the
/// backlog budget (`MAX_BACKLOG_PACKETS` packets of up to
/// [`DEFAULT_MSS`] each).
pub const RECEIVE_WINDOW: u32 = 372_352;

/// The initial congestion window in bytes (`4 × DEFAULT_MSS`): growth is
/// additive per acked byte, loss halves the window.
pub const INITIAL_CONGESTION_WINDOW: u32 = 5_808;

/// The floor the congestion window never drops below, even after repeated
/// timeout passes (`DEFAULT_MSS` in bytes): one segment always fits.
pub const MIN_CONGESTION_WINDOW: u32 = 1_452;

/// The ceiling the congestion window never grows beyond (4 MiB): a
/// runaway window would defeat the bounded-memory house rule.
pub const MAX_CONGESTION_WINDOW: u32 = 4_194_304;

/// Extension byte meaning "no more extensions follow" (BEP 29).
pub const EXT_NONE: u8 = 0;

/// Extension id of the selective-ack frame (BEP 34): a little-endian
/// bitmask whose bit `i` acks `ack_nr + 1 + i`.
pub const EXT_SELECTIVE_ACK: u8 = 1;

/// One uTP packet's type, carried in the high nibble of the first header
/// byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    /// `ST_DATA` — payload bytes.
    Data,
    /// `ST_FIN` — the sender will send nothing more.
    Fin,
    /// `ST_STATE` — a pure ack, no payload.
    State,
    /// `ST_RESET` — kill the connection.
    Reset,
    /// `ST_SYN` — open the connection.
    Syn,
}

impl PacketType {
    /// The type nibble a header carries for this packet type.
    #[must_use]
    pub const fn nibble(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Fin => 1,
            Self::State => 2,
            Self::Reset => 3,
            Self::Syn => 4,
        }
    }

    /// Decodes a type nibble; anything above [`PacketType::Syn`] is a
    /// typed error, not a panic.
    ///
    /// # Errors
    /// Returns [`Error::Utp`] for an unassigned type nibble.
    pub fn from_nibble(nibble: u8) -> Result<Self> {
        match nibble {
            0 => Ok(Self::Data),
            1 => Ok(Self::Fin),
            2 => Ok(Self::State),
            3 => Ok(Self::Reset),
            4 => Ok(Self::Syn),
            other => Err(Error::Utp(format!(
                "packet type {other} is unassigned; BEP 29 defines types 0-4"
            ))),
        }
    }
}

/// The fixed 20-byte uTP header, all fields big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// What the packet carries.
    pub packet_type: PacketType,
    /// The connection id this packet travels under: a connection's two
    /// directions use consecutive ids (`send_id` one way, `send_id + 1`
    /// back).
    pub connection_id: u16,
    /// The sender's current microsecond timestamp.
    pub timestamp_us: u32,
    /// The sender's estimate of the receiver's timestamp minus its own —
    /// the delay sample LEDBAT-like controllers consume.
    pub timestamp_diff_us: u32,
    /// The sender's advertised receive window in bytes.
    pub window_size: u32,
    /// The sender's sequence number for this packet.
    pub seq_nr: u16,
    /// The highest in-order sequence number the sender has received.
    pub ack_nr: u16,
}

impl Header {
    /// Appends the fixed 20-byte wire form to `out`. The wire's second
    /// byte is the extension pointer; a bare header always encodes it as
    /// [`EXT_NONE`] — packets with extensions are written by
    /// [`Packet::encode_into`], which fills it in.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push((self.packet_type.nibble() << 4) | VERSION);
        out.push(EXT_NONE);
        out.extend_from_slice(&self.connection_id.to_be_bytes());
        out.extend_from_slice(&self.timestamp_us.to_be_bytes());
        out.extend_from_slice(&self.timestamp_diff_us.to_be_bytes());
        out.extend_from_slice(&self.window_size.to_be_bytes());
        out.extend_from_slice(&self.seq_nr.to_be_bytes());
        out.extend_from_slice(&self.ack_nr.to_be_bytes());
    }

    /// The fixed 20-byte wire form.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = Vec::with_capacity(HEADER_LEN);
        self.encode_into(&mut out);
        let mut wire = [0_u8; HEADER_LEN];
        wire.copy_from_slice(&out);
        wire
    }

    /// Parses exactly one fixed header from the front of `bytes`.
    ///
    /// # Errors
    /// Returns [`Error::Utp`] when fewer than [`HEADER_LEN`] bytes remain,
    /// the version nibble is not [`VERSION`], or the type nibble is
    /// unassigned.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let wire = fixed_header(bytes)?;
        let version = wire[0] & 0x0F;
        if version != VERSION {
            return Err(Error::Utp(format!(
                "uTP version {version} is not spoken here; the wire speaks {VERSION}"
            )));
        }
        let packet_type = PacketType::from_nibble(wire[0] >> 4)?;
        Ok(Self {
            packet_type,
            connection_id: u16::from_be_bytes([wire[2], wire[3]]),
            timestamp_us: u32::from_be_bytes([wire[4], wire[5], wire[6], wire[7]]),
            timestamp_diff_us: u32::from_be_bytes([wire[8], wire[9], wire[10], wire[11]]),
            window_size: u32::from_be_bytes([wire[12], wire[13], wire[14], wire[15]]),
            seq_nr: u16::from_be_bytes([wire[16], wire[17]]),
            ack_nr: u16::from_be_bytes([wire[18], wire[19]]),
        })
    }
}

/// One link of a packet's extension chain: `kind` names the extension
/// ([`EXT_SELECTIVE_ACK`] today) and `contents` is its bounded byte
/// string, borrowed from the input when parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension<'a> {
    /// The extension's registered id.
    pub kind: u8,
    /// The extension's contents, borrowed from the input when parsed.
    pub contents: Cow<'a, [u8]>,
}

impl Extension<'_> {
    /// The sequence numbers this selective-ack frame acks, given the base
    /// `ack_nr` the carrying header reports: bit `i` of the bitmask acks
    /// `base + 1 + i` (BEP 34), walking each byte little-endian within
    /// itself.
    #[must_use]
    pub fn selective_acked(&self, base: u16) -> Vec<u16> {
        let mut acked = Vec::new();
        for (byte_index, byte) in self.contents.iter().enumerate() {
            for bit in 0..8_u16 {
                if byte & (1 << bit) != 0 {
                    let index = u16::try_from(byte_index)
                        .ok()
                        .and_then(|base_index| base_index.checked_mul(8))
                        .and_then(|base_index| base_index.checked_add(bit));
                    if let Some(index) = index {
                        acked.push(base.wrapping_add(1).wrapping_add(index));
                    }
                }
            }
        }
        acked
    }
}

/// One parsed uTP packet: the fixed header, the extension chain, and the
/// remaining payload bytes, borrowed from the input where possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<'a> {
    /// The fixed header fields.
    pub header: Header,
    /// The extension chain, in wire order.
    pub extensions: Vec<Extension<'a>>,
    /// The packet's payload (`ST_DATA` bytes, empty for control types).
    pub payload: Cow<'a, [u8]>,
}

impl<'p> Packet<'p> {
    /// Appends the wire form — header with its extension pointer, the
    /// extension chain, the payload — to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push((self.header.packet_type.nibble() << 4) | VERSION);
        out.push(
            self.extensions
                .first()
                .map_or(EXT_NONE, |extension| extension.kind),
        );
        out.extend_from_slice(&self.header.connection_id.to_be_bytes());
        out.extend_from_slice(&self.header.timestamp_us.to_be_bytes());
        out.extend_from_slice(&self.header.timestamp_diff_us.to_be_bytes());
        out.extend_from_slice(&self.header.window_size.to_be_bytes());
        out.extend_from_slice(&self.header.seq_nr.to_be_bytes());
        out.extend_from_slice(&self.header.ack_nr.to_be_bytes());
        // Each link names the id of the NEXT extension, terminating on
        // zero; the length byte caps one extension at 255 bytes (the
        // parser cannot produce anything longer, and a hand-built packet
        // beyond that clamps rather than panics).
        for (index, extension) in self.extensions.iter().enumerate() {
            let next = self
                .extensions
                .get(index + 1)
                .map_or(EXT_NONE, |extension| extension.kind);
            out.push(next);
            out.push(u8::try_from(extension.contents.len()).unwrap_or(u8::MAX));
            out.extend_from_slice(&extension.contents);
        }
        out.extend_from_slice(&self.payload);
    }

    /// The wire form.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Parses one packet out of a UDP datagram.
    ///
    /// # Errors
    /// Returns [`Error::Utp`] when the header is malformed (see
    /// [`Header::parse`]) or an extension run overruns the datagram.
    pub fn parse(bytes: &'p [u8]) -> Result<Self> {
        let header = Header::parse(bytes)?;
        let mut kind = bytes[1];
        let mut rest = &bytes[HEADER_LEN..];
        let mut extensions = Vec::new();
        while kind != EXT_NONE {
            let Some((link, tail)) = rest.split_first_chunk::<2>() else {
                return Err(Error::Utp(format!(
                    "extension {kind} claims a link that overruns the datagram"
                )));
            };
            let (next, len) = (link[0], usize::from(link[1]));
            let Some(contents) = tail.get(..len) else {
                return Err(Error::Utp(format!(
                    "extension {kind} claims {len} bytes that overrun the datagram"
                )));
            };
            extensions.push(Extension {
                kind,
                contents: Cow::Borrowed(contents),
            });
            kind = next;
            rest = &tail[len..];
        }
        Ok(Self {
            header,
            extensions,
            payload: Cow::Borrowed(rest),
        })
    }
}

/// Parses one packet out of a UDP datagram.
///
/// # Errors
/// Returns [`Error::Utp`] when the header is malformed (see
/// [`Header::parse`]) or an extension run overruns the datagram.
pub fn parse_packet(bytes: &[u8]) -> Result<Packet<'_>> {
    Packet::parse(bytes)
}

/// Where a connection sits in the uTP handshake and teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// The initiator sent its `ST_SYN` and awaits the peer's `ST_STATE`.
    SynSent,
    /// The listener accepted a `ST_SYN` and awaits traffic confirming the
    /// peer saw our `ST_STATE`.
    SynReceived,
    /// Both sides sequence traffic normally.
    Connected,
    /// This side sent its `ST_FIN`; outstanding data may still arrive.
    FinSent,
    /// The peer's `ST_FIN` closed the connection (or ours was acked).
    Closed,
    /// The peer sent `ST_RESET`; the connection is dead.
    Reset,
}

/// One inbound delivery, or the bookkeeping a datagram produced. A
/// receive call yields the datagram's own event first, then anything the
/// receive backlog unlocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// In-order `ST_DATA` bytes, delivered to the caller.
    Data {
        /// The packet's sequence number.
        seq_nr: u16,
        /// The delivered bytes (borrowed from the datagram when fresh,
        /// owned when drained from the backlog).
        payload: Vec<u8>,
    },
    /// A future sequence number: buffered until the gap fills (bounded by
    /// [`MAX_BACKLOG_PACKETS`]).
    Buffered {
        /// The buffered packet's sequence number.
        seq_nr: u16,
        /// How many bytes are waiting.
        len: usize,
    },
    /// An `ST_STATE` ack: the sequence numbers it retired, cumulative
    /// (everything at or below `ack_nr`) plus selective-ack bits.
    Ack {
        /// The retired sequence numbers.
        acked: Vec<u16>,
    },
    /// The peer's `ST_FIN`: nothing more follows from their side.
    Fin {
        /// The FIN's sequence number.
        seq_nr: u16,
    },
    /// The peer sent `ST_RESET`; the connection is dead.
    Reset,
    /// A duplicate, an out-of-window sequence number, a foreign
    /// connection id, or a stray post-handshake `ST_SYN` — adversarial or
    /// redundant wire data, answered with silence, never an error.
    Stale,
}

/// One un-acked outbound packet, kept for retransmission and window
/// bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFlight {
    seq_nr: u16,
    len: u32,
    sent_us: u32,
}

/// The runtime-free uTP connection: sequence and ack bookkeeping, window
/// pacing, LEDBAT-flavored delay samples, and the state machine — with no
/// socket, no clock, and no executor in sight.
#[derive(Debug, Clone)]
pub struct Connection {
    state: ConnectionState,
    send_id: u16,
    recv_id: u16,
    seq_nr: u16,
    their_seq: u16,
    inflight: VecDeque<InFlight>,
    their_window: u32,
    congestion_window: u32,
    base_delay_us: u32,
    rto_us: u32,
    backlog: BTreeMap<u16, Vec<u8>>,
}

impl Connection {
    /// A fresh initiator connection: `send_id` is the caller's random
    /// connection id (the wire's SYN carries it), the return direction
    /// listens on `send_id + 1`, and sequence numbers start at 1. The
    /// SYN's in-flight bookkeeping lands in [`Connection::syn_packet`],
    /// when the bytes actually leave.
    #[must_use]
    pub fn connect(send_id: u16) -> Self {
        Self {
            state: ConnectionState::SynSent,
            send_id,
            recv_id: send_id.wrapping_add(1),
            seq_nr: 1,
            their_seq: 0,
            inflight: VecDeque::new(),
            their_window: 0,
            congestion_window: INITIAL_CONGESTION_WINDOW,
            base_delay_us: u32::MAX,
            rto_us: DEFAULT_RTO_US,
            backlog: BTreeMap::new(),
        }
    }

    /// The `ST_SYN` packet that opens this connection. The SYN consumes
    /// sequence number 1 and books itself in flight, so the peer's ack
    /// retires it and the first data packet follows on sequence 2.
    pub fn syn_packet(&mut self, timestamp_us: u32) -> Packet<'static> {
        self.register_in_flight(1, 0, timestamp_us);
        self.seq_nr = 2;
        Packet {
            header: Header {
                packet_type: PacketType::Syn,
                connection_id: self.send_id,
                timestamp_us,
                timestamp_diff_us: 0,
                window_size: RECEIVE_WINDOW,
                seq_nr: 1,
                ack_nr: 0,
            },
            extensions: Vec::new(),
            payload: Cow::Owned(Vec::new()),
        }
    }

    /// Accepts an inbound `ST_SYN`: the listener listens on the SYN's
    /// connection id and sends on the next one, mirroring the initiator.
    /// The SYN's sequence number becomes the listener's expectation, so
    /// the peer's first data packet must follow it — including across the
    /// u16 wraparound edge.
    #[must_use]
    pub fn accept_syn(syn: &Header) -> Self {
        Self {
            state: ConnectionState::SynReceived,
            send_id: syn.connection_id.wrapping_add(1),
            recv_id: syn.connection_id,
            seq_nr: 1,
            their_seq: syn.seq_nr,
            inflight: VecDeque::new(),
            their_window: 0,
            congestion_window: INITIAL_CONGESTION_WINDOW,
            base_delay_us: u32::MAX,
            rto_us: DEFAULT_RTO_US,
            backlog: BTreeMap::new(),
        }
    }

    /// The pure-ack `ST_STATE` that answers the peer with our current
    /// in-order position and advertised window. State packets carry the
    /// current sequence number but consume none (BEP 29).
    #[must_use]
    pub fn state_packet(&self, timestamp_us: u32) -> Packet<'static> {
        Packet {
            header: Header {
                packet_type: PacketType::State,
                connection_id: self.send_id,
                timestamp_us,
                timestamp_diff_us: 0,
                window_size: RECEIVE_WINDOW,
                seq_nr: self.seq_nr,
                ack_nr: self.their_seq,
            },
            extensions: Vec::new(),
            payload: Cow::Owned(Vec::new()),
        }
    }

    /// Where the connection sits in the handshake and teardown.
    #[must_use]
    pub const fn state(&self) -> ConnectionState {
        self.state
    }

    /// The connection id our outbound packets travel under.
    #[must_use]
    pub const fn send_id(&self) -> u16 {
        self.send_id
    }

    /// The connection id we listen on for the peer's packets.
    #[must_use]
    pub const fn recv_id(&self) -> u16 {
        self.recv_id
    }

    /// The bytes we may still put on the wire: the smaller of the peer's
    /// advertised window and our congestion window, minus bytes already
    /// in flight.
    #[must_use]
    pub fn usable_window(&self) -> u32 {
        let in_flight_bytes: u32 = self.inflight.iter().map(|packet| packet.len).sum();
        self.congestion_window
            .min(self.their_window)
            .saturating_sub(in_flight_bytes)
    }

    /// How many outbound packets are awaiting an ack.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// The smallest delay sample seen so far (LEDBAT's base delay), in
    /// microseconds; `u32::MAX` before any sample.
    #[must_use]
    pub const fn base_delay_us(&self) -> u32 {
        self.base_delay_us
    }

    /// Packetizes `payload` into [`DEFAULT_MSS`]-sized `ST_DATA` packets,
    /// pacing by [`Connection::usable_window`]; bytes beyond the window
    /// are left for the next call. Every packet consumes one sequence
    /// number and registers in flight for [`Connection::timed_out`].
    ///
    /// Sending after our own [`Connection::fin_packet`] (or a reset) is a
    /// no-op: the wire is closed for data.
    #[must_use]
    pub fn send(&mut self, payload: &[u8], timestamp_us: u32) -> Vec<Packet<'static>> {
        let mut packets = Vec::new();
        if matches!(
            self.state,
            ConnectionState::FinSent | ConnectionState::Closed | ConnectionState::Reset
        ) {
            return packets;
        }
        for chunk in payload.chunks(DEFAULT_MSS) {
            let len = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
            if self.usable_window() < len {
                break;
            }
            let seq_nr = self.seq_nr;
            self.seq_nr = self.seq_nr.wrapping_add(1);
            self.register_in_flight(seq_nr, len, timestamp_us);
            packets.push(Packet {
                header: Header {
                    packet_type: PacketType::Data,
                    connection_id: self.send_id,
                    timestamp_us,
                    timestamp_diff_us: 0,
                    window_size: RECEIVE_WINDOW,
                    seq_nr,
                    ack_nr: self.their_seq,
                },
                extensions: Vec::new(),
                payload: Cow::Owned(chunk.to_vec()),
            });
        }
        packets
    }

    /// The `ST_FIN` that closes our side of the wire; it consumes a
    /// sequence number like data does and registers in flight — the peer
    /// must ack it.
    #[must_use]
    pub fn fin_packet(&mut self, timestamp_us: u32) -> Packet<'static> {
        let seq_nr = self.seq_nr;
        self.seq_nr = self.seq_nr.wrapping_add(1);
        self.register_in_flight(seq_nr, 0, timestamp_us);
        self.state = ConnectionState::FinSent;
        Packet {
            header: Header {
                packet_type: PacketType::Fin,
                connection_id: self.send_id,
                timestamp_us,
                timestamp_diff_us: 0,
                window_size: RECEIVE_WINDOW,
                seq_nr,
                ack_nr: self.their_seq,
            },
            extensions: Vec::new(),
            payload: Cow::Owned(Vec::new()),
        }
    }

    /// Consumes one inbound datagram and yields every delivery it
    /// unlocks: the datagram's own event first, then any backlog that
    /// became in-order. Adversarial bytes (wrong connection id, repeats,
    /// out-of-window sequence numbers) answer as [`Incoming::Stale`] —
    /// data, not a crash.
    ///
    /// # Errors
    /// Returns [`Error::Utp`] when the datagram is not a decodable uTP
    /// packet at all (see [`Packet::parse`]).
    pub fn receive(&mut self, wire: &[u8], now_us: u32) -> Result<Vec<Incoming>> {
        let packet = Packet::parse(wire)?;
        // A datagram for another connection id is someone else's traffic:
        // silence (BEP 29 drops these).
        if packet.header.connection_id != self.recv_id {
            return Ok(vec![Incoming::Stale]);
        }
        self.their_window = packet.header.window_size;
        self.update_base_delay(packet.header.timestamp_us, now_us);

        // Handshake completion rides the ack pointer: the initiator
        // connects when a state packet confirms its SYN, and the listener
        // connects once the peer answers our state answer.
        if self.state == ConnectionState::SynSent
            && packet.header.packet_type == PacketType::State
            && seq_at_or_before(1, packet.header.ack_nr)
        {
            self.state = ConnectionState::Connected;
        }
        if self.state == ConnectionState::SynReceived
            && matches!(
                packet.header.packet_type,
                PacketType::State | PacketType::Data | PacketType::Fin
            )
        {
            self.state = ConnectionState::Connected;
        }

        let mut events = Vec::new();
        match packet.header.packet_type {
            // A post-handshake SYN is a replay of the handshake: silence.
            PacketType::Syn => events.push(Incoming::Stale),
            PacketType::State => {
                let acked = self.process_acks(packet.header.ack_nr, &packet.extensions);
                events.push(Incoming::Ack { acked });
            }
            PacketType::Data => self.receive_data(&packet, &mut events),
            PacketType::Fin => {
                let seq_nr = packet.header.seq_nr;
                if seq_nr == self.their_seq.wrapping_add(1) {
                    self.their_seq = seq_nr;
                    events.push(Incoming::Fin { seq_nr });
                    self.state = ConnectionState::Closed;
                } else {
                    events.push(Incoming::Stale);
                }
            }
            PacketType::Reset => {
                self.state = ConnectionState::Reset;
                events.push(Incoming::Reset);
            }
        }
        Ok(events)
    }

    /// The outbound sequence numbers whose retransmission timeout has
    /// passed: the caller resends their bytes. Each timeout pass doubles
    /// the RTO and halves the congestion window (loss = backoff).
    #[must_use]
    pub fn timed_out(&mut self, now_us: u32) -> Vec<u16> {
        let mut timed = Vec::new();
        let mut index = 0;
        while index < self.inflight.len() {
            let packet = self.inflight[index];
            if now_us.wrapping_sub(packet.sent_us) >= self.rto_us {
                timed.push(packet.seq_nr);
                self.inflight.remove(index);
            } else {
                index += 1;
            }
        }
        if !timed.is_empty() {
            self.rto_us = self.rto_us.saturating_mul(2);
            self.congestion_window = (self.congestion_window / 2).max(MIN_CONGESTION_WINDOW);
        }
        timed
    }

    #[cfg(test)]
    fn seq_hint(&self) -> u16 {
        self.seq_nr
    }

    #[cfg(test)]
    fn their_seq_hint(&self) -> u16 {
        self.their_seq
    }

    /// Books one outbound packet (or the seeded SYN/FIN) as awaiting its
    /// ack.
    fn register_in_flight(&mut self, seq_nr: u16, len: u32, sent_us: u32) {
        self.inflight.push_back(InFlight {
            seq_nr,
            len,
            sent_us,
        });
    }

    /// Retires acked outbound packets — everything at or below the
    /// cumulative `ack_nr`, plus any selective-ack bits — and grows the
    /// congestion window additively, by the bytes each retired packet
    /// carried.
    fn process_acks(&mut self, ack_nr: u16, extensions: &[Extension<'_>]) -> Vec<u16> {
        let outstanding: Vec<InFlight> = self.inflight.drain(..).collect();
        let mut acked = Vec::new();
        let mut acked_bytes = 0_u32;
        for packet in outstanding {
            if seq_at_or_before(packet.seq_nr, ack_nr) {
                acked_bytes += packet.len;
                acked.push(packet.seq_nr);
            } else {
                self.inflight.push_back(packet);
            }
        }
        for extension in extensions
            .iter()
            .filter(|extension| extension.kind == EXT_SELECTIVE_ACK)
        {
            for seq_nr in extension.selective_acked(ack_nr) {
                if let Some(index) = self
                    .inflight
                    .iter()
                    .position(|packet| packet.seq_nr == seq_nr)
                {
                    acked_bytes += self.inflight[index].len;
                    self.inflight.remove(index);
                    acked.push(seq_nr);
                }
            }
        }
        if !acked.is_empty() {
            self.congestion_window =
                (self.congestion_window + acked_bytes).min(MAX_CONGESTION_WINDOW);
        }
        acked
    }

    /// Delivers in-order data, buffers bounded future packets, answers
    /// repeats with silence.
    fn receive_data(&mut self, packet: &Packet<'_>, events: &mut Vec<Incoming>) {
        let seq_nr = packet.header.seq_nr;
        if seq_nr == self.their_seq.wrapping_add(1) {
            events.push(Incoming::Data {
                seq_nr,
                payload: packet.payload.to_vec(),
            });
            self.their_seq = seq_nr;
            self.drain_backlog(events);
        } else if seq_at_or_before(seq_nr, self.their_seq)
            || self.backlog.len() >= MAX_BACKLOG_PACKETS
            || seq_nr.wrapping_sub(self.their_seq) > MAX_RECEIVE_WINDOW
        {
            events.push(Incoming::Stale);
        } else {
            let len = packet.payload.len();
            self.backlog.insert(seq_nr, packet.payload.to_vec());
            events.push(Incoming::Buffered { seq_nr, len });
        }
    }

    /// Delivers every backlog packet the last in-order advance unlocked,
    /// walking sequence numbers with u16 wraparound.
    fn drain_backlog(&mut self, events: &mut Vec<Incoming>) {
        loop {
            let next = self.their_seq.wrapping_add(1);
            let Some(payload) = self.backlog.remove(&next) else {
                break;
            };
            events.push(Incoming::Data {
                seq_nr: next,
                payload,
            });
            self.their_seq = next;
        }
    }

    /// Folds one delay sample into the LEDBAT base delay: the minimum of
    /// (our receive time − their send timestamp) seen so far, computed
    /// with u32 wraparound tolerance.
    fn update_base_delay(&mut self, their_timestamp_us: u32, now_us: u32) {
        let sample = now_us.wrapping_sub(their_timestamp_us);
        if sample < self.base_delay_us {
            self.base_delay_us = sample;
        }
    }
}

/// `seq` counts as already received (or acked) when it sits at or before
/// `ack` in u16 sequence space — the wraparound window BEP 29 fixes at
/// less than half the space.
fn seq_at_or_before(seq_nr: u16, ack_nr: u16) -> bool {
    ack_nr.wrapping_sub(seq_nr) < 0x8000
}

/// Borrows exactly [`HEADER_LEN`] bytes, or fails with the shared
/// truncation error.
fn fixed_header(bytes: &[u8]) -> Result<&[u8; HEADER_LEN]> {
    let wire = bytes
        .get(..HEADER_LEN)
        .and_then(|prefix| <&[u8; HEADER_LEN]>::try_from(prefix).ok());
    wire.ok_or_else(|| {
        Error::Utp(format!(
            "uTP header needs {HEADER_LEN} bytes, got {}",
            bytes.len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical header: `ST_DATA` on connection `0xBEEF`, seq 1000
    /// acking 999, advertising 64 KiB.
    fn sample_header() -> Header {
        Header {
            packet_type: PacketType::Data,
            connection_id: 0xBEEF,
            timestamp_us: 1_700_000_000,
            timestamp_diff_us: 1_234,
            window_size: 65_535,
            seq_nr: 1000,
            ack_nr: 999,
        }
    }

    /// ARCHITECTURE.md §9 pins the acceptance criterion this module lands:
    /// the M2 row's "uTP (BEP 29)" leg — the fixed 20-byte header parses
    /// back byte-for-byte from its canonical wire form.
    #[test]
    fn header_round_trips_byte_for_byte() {
        let header = sample_header();
        let wire = header.encode();
        assert_eq!(wire.len(), HEADER_LEN);
        // type 0 (ST_DATA) | version 1 in byte 0; then id, timestamps,
        // window, seq, ack — all big-endian.
        assert_eq!(
            wire,
            [
                0x01, 0x00, 0xBE, 0xEF, 0x65, 0x53, 0xF1, 0x00, 0x00, 0x00, 0x04, 0xD2, 0x00, 0x00,
                0xFF, 0xFF, 0x03, 0xE8, 0x03, 0xE7,
            ]
        );
        assert_eq!(Header::parse(&wire).unwrap(), header);
    }

    #[test]
    fn header_parse_rejects_truncated_versions_and_unknown_types() {
        let wire = sample_header().encode();
        assert!(Header::parse(&wire[..HEADER_LEN - 1]).is_err());
        // Version nibble 2 is not spoken here.
        let mut foreign = wire;
        foreign[0] = 0x02;
        let err = Header::parse(&foreign).expect_err("version 2");
        assert!(err.to_string().contains("version"), "got {err}");
        // Type nibble 5 is unassigned.
        let mut hostile = wire;
        hostile[0] = 0x51;
        assert!(Header::parse(&hostile).is_err());
    }

    #[test]
    fn packet_extension_chain_round_trips_and_selective_acks_decode() {
        // A selective-ack bitmask: bits 0, 1 and 9 set → ack_nr+1, +2, +10.
        let mask = [0b0000_0011_u8, 0b0000_0100, 0b0000_0000];
        let packet = Packet {
            header: sample_header(),
            extensions: vec![Extension {
                kind: EXT_SELECTIVE_ACK,
                contents: Cow::Owned(mask.to_vec()),
            }],
            payload: Cow::Owned(b"payload".to_vec()),
        };
        let wire = packet.encode();
        let parsed = Packet::parse(&wire).expect("legal packet");
        assert_eq!(parsed.header, packet.header);
        assert_eq!(parsed.payload, packet.payload);
        assert_eq!(parsed.extensions.len(), 1);
        assert_eq!(
            parsed.extensions[0].selective_acked(999),
            vec![1000, 1001, 1010]
        );
    }

    #[test]
    fn extension_runs_that_overrun_the_datagram_are_typed_errors() {
        // The extension claims 40 bytes but the datagram ends after 4.
        let mut wire = sample_header().encode().to_vec();
        wire[1] = EXT_SELECTIVE_ACK;
        wire.push(EXT_NONE);
        wire.push(40);
        wire.extend_from_slice(&[1, 2, 3, 4]);
        let err = Packet::parse(&wire).expect_err("overrunning extension");
        assert!(err.to_string().contains("extension"), "got {err}");
    }

    #[test]
    fn connect_sends_syn_and_accept_syn_opens_the_session() {
        // Initiator: random send id R, listens on R+1, seq starts at 1.
        let mut initiator = Connection::connect(0x1234);
        assert_eq!(initiator.send_id(), 0x1234);
        assert_eq!(initiator.recv_id(), 0x1235);
        assert_eq!(initiator.state(), ConnectionState::SynSent);

        let syn = initiator.syn_packet(0);
        assert_eq!(syn.header.packet_type, PacketType::Syn);
        assert_eq!(syn.header.connection_id, 0x1234);
        assert_eq!(syn.header.seq_nr, 1);

        // Listener: accepts the SYN, answers with a pure ack of seq 1,
        // and sends on R+1 while listening on R.
        let listener = Connection::accept_syn(&syn.header);
        assert_eq!(listener.recv_id(), 0x1234);
        assert_eq!(listener.send_id(), 0x1235);
        assert_eq!(listener.state(), ConnectionState::SynReceived);
        let answer = listener.state_packet(1);
        assert_eq!(answer.header.packet_type, PacketType::State);
        assert_eq!(answer.header.connection_id, 0x1235);
        assert_eq!(answer.header.ack_nr, 1);
    }

    #[test]
    fn the_initiator_connects_when_the_syn_is_acked() {
        let mut initiator = Connection::connect(0x1234);
        let syn = initiator.syn_packet(0);
        let listener = Connection::accept_syn(&syn.header);
        let answer = listener.state_packet(1);
        let events = initiator.receive(&answer.encode(), 2).expect("legal ack");
        assert_eq!(events, vec![Incoming::Ack { acked: vec![1] }]);
        assert_eq!(initiator.state(), ConnectionState::Connected);
    }

    #[test]
    fn send_packetizes_by_mss_and_paces_by_the_window() {
        let mut connection = handshake();
        // 4 KiB of payload tiles into three ST_DATA packets.
        let payload = vec![7_u8; 4_096];
        let packets = connection.send(&payload, 100);
        assert_eq!(packets.len(), 3);
        assert!(
            packets
                .iter()
                .all(|p| p.header.packet_type == PacketType::Data)
        );
        assert_eq!(packets[0].header.seq_nr, 2);
        assert_eq!(packets[1].header.seq_nr, 3);
        assert_eq!(packets[2].header.seq_nr, 4);
        let total: usize = packets.iter().map(|p| p.payload.len()).sum();
        assert_eq!(total, 4_096);
        assert_eq!(connection.in_flight(), 3);
        assert!(packets[0].payload.len() <= DEFAULT_MSS);

        // The peer's advertised window (4 KiB on the answering state) plus
        // the congestion window bound further sends: everything in flight
        // consumed the usable window, so the next call sends nothing.
        let more = connection.send(&payload, 101);
        assert!(more.is_empty(), "the usable window is spent");
    }

    #[test]
    fn receive_delivers_in_order_data_and_buffers_the_rest() {
        let (mut sender, mut receiver) = pair();
        // seq 2 arrives in order…
        let first = sender.send(&[1_u8; 16], 0).remove(0);
        let events = receiver.receive(&first.encode(), 10).expect("legal data");
        assert_eq!(
            events,
            vec![Incoming::Data {
                seq_nr: 2,
                payload: vec![1_u8; 16],
            }]
        );
        // …seq 4 jumps the gap and buffers; seq 3 then fills it.
        let mut packets = sender.send(&[2_u8; 16], 1);
        packets.extend(sender.send(&[3_u8; 16], 2));
        let (future, middle) = (&packets[1], &packets[0]);
        let events = receiver.receive(&future.encode(), 11).expect("legal data");
        assert_eq!(
            events,
            vec![Incoming::Buffered {
                seq_nr: future.header.seq_nr,
                len: 16,
            }]
        );
        let events = receiver.receive(&middle.encode(), 12).expect("legal data");
        assert_eq!(events.len(), 2, "the gap fill unlocks the backlog");
        assert_eq!(
            events[0],
            Incoming::Data {
                seq_nr: middle.header.seq_nr,
                payload: vec![2_u8; 16],
            }
        );
        assert_eq!(
            events[1],
            Incoming::Data {
                seq_nr: future.header.seq_nr,
                payload: vec![3_u8; 16],
            }
        );
    }

    #[test]
    fn duplicates_foreign_ids_and_far_futures_are_stale_data() {
        let (mut sender, mut receiver) = pair();
        let packet = sender.send(&[1_u8; 8], 0).remove(0);
        receiver
            .receive(&packet.encode(), 10)
            .expect("first delivery");

        // The same seq again: a duplicate, answered with silence.
        let events = receiver.receive(&packet.encode(), 11).expect("parseable");
        assert_eq!(events, vec![Incoming::Stale]);

        // A datagram for a different connection id: not ours.
        let mut foreign = sample_header();
        foreign.packet_type = PacketType::Data;
        foreign.seq_nr = 999;
        let events = receiver
            .receive(&wire_packet(&foreign, b"x"), 12)
            .expect("parseable");
        assert_eq!(events, vec![Incoming::Stale]);

        // A sequence number beyond the receive window: dropped as stale.
        let mut far = sample_header();
        far.packet_type = PacketType::Data;
        far.seq_nr = receiver_window_edge(&receiver) + 5_000;
        let events = receiver
            .receive(&wire_packet(&far, b"y"), 13)
            .expect("parseable");
        assert_eq!(events, vec![Incoming::Stale]);
    }

    #[test]
    fn fin_closes_and_reset_resets() {
        let (mut sender, mut receiver) = pair();
        let fin = sender.fin_packet(0);
        assert_eq!(fin.header.packet_type, PacketType::Fin);
        assert_eq!(sender.state(), ConnectionState::FinSent);
        // FIN consumes a sequence number like data does.
        assert_eq!(fin.header.seq_nr, 2);

        let events = receiver.receive(&fin.encode(), 10).expect("legal fin");
        assert_eq!(events, vec![Incoming::Fin { seq_nr: 2 }]);
        assert_eq!(receiver.state(), ConnectionState::Closed);

        // A reset from the peer kills the connection regardless.
        let (a, mut b) = pair();
        let reset = Packet {
            header: Header {
                packet_type: PacketType::Reset,
                connection_id: b.recv_id(),
                timestamp_us: 0,
                timestamp_diff_us: 0,
                window_size: 0,
                seq_nr: a.seq_hint(),
                ack_nr: b.their_seq_hint(),
            },
            extensions: Vec::new(),
            payload: Cow::Borrowed(&[][..]),
        };
        let events = b.receive(&reset.encode(), 5).expect("legal reset");
        assert_eq!(events, vec![Incoming::Reset]);
        assert_eq!(b.state(), ConnectionState::Reset);
    }

    #[test]
    fn timed_out_doubles_the_rto_and_halves_the_window() {
        let mut connection = handshake();
        let payload = vec![7_u8; DEFAULT_MSS];
        let packets = connection.send(&payload, 1_000_000);
        assert_eq!(packets.len(), 1);
        // Nothing timed out yet: the RTO has not passed.
        assert_eq!(connection.timed_out(1_400_000), Vec::<u16>::new());
        let timed = connection.timed_out(1_600_000);
        assert_eq!(timed, vec![2]);
        // One timeout pass: the RTO doubles and the congestion window
        // halves (loss = backoff).
        assert_eq!(connection.timed_out(1_700_000), Vec::<u16>::new());
    }

    #[test]
    fn delay_samples_track_the_ledbat_base_delay() {
        let (mut sender, receiver) = pair();
        // The peer's timestamps drift downward: the base delay keeps the
        // minimum sample.
        let mut answer = receiver.state_packet(1);
        answer.header.timestamp_us = 10_000;
        sender.receive(&answer.encode(), 11_500).expect("legal ack");
        assert_eq!(sender.base_delay_us(), 1_500);
        answer.header.timestamp_us = 10_500;
        sender.receive(&answer.encode(), 11_600).expect("legal ack");
        // 1_600 - 10_500 = 1_100 µs: a new minimum.
        assert_eq!(sender.base_delay_us(), 1_100);
        answer.header.timestamp_us = 10_000;
        sender.receive(&answer.encode(), 11_700).expect("legal ack");
        // 1_700 µs is worse than the minimum: base delay does not move.
        assert_eq!(sender.base_delay_us(), 1_100);
    }

    #[test]
    fn acks_grow_the_congestion_window_additively() {
        let (mut sender, receiver) = pair();
        let sent = sender.send(&[7_u8; 64], 0);
        assert_eq!(sent.len(), 1);
        let before = sender.usable_window();
        // The peer acks seq 2 (the first data packet after the handshake's
        // seq-1 SYN).
        let mut ack = receiver.state_packet(1);
        ack.header.ack_nr = sent[0].header.seq_nr;
        sender.receive(&ack.encode(), 10).expect("legal ack");
        // The 64 released bytes plus the 64 additive-growth bytes the ack
        // credits: the usable window widens by both.
        assert_eq!(sender.usable_window(), before + 128);
        assert_eq!(sender.in_flight(), 0);
    }

    #[test]
    fn sequence_numbers_wrap_u16_style() {
        let mut sender = Connection::connect(7);
        let mut syn = sender.syn_packet(0);
        // A SYN at the sequence-space edge: the listener's next expected
        // seq is 0, wrapped past u16::MAX.
        syn.header.seq_nr = u16::MAX;
        let mut listener = Connection::accept_syn(&syn.header);
        let data = Packet {
            header: Header {
                packet_type: PacketType::Data,
                connection_id: listener.recv_id(),
                timestamp_us: 0,
                timestamp_diff_us: 0,
                window_size: 1 << 20,
                seq_nr: 0,
                ack_nr: 1,
            },
            extensions: Vec::new(),
            payload: Cow::Borrowed(b"wrapped"),
        };
        let events = listener.receive(&data.encode(), 2).expect("legal data");
        assert_eq!(
            events,
            vec![Incoming::Data {
                seq_nr: 0,
                payload: b"wrapped".to_vec(),
            }]
        );
    }

    /// An initiator and a listener past their handshake: the initiator is
    /// connected (its SYN acked), the listener has answered the SYN.
    fn pair() -> (Connection, Connection) {
        let mut initiator = Connection::connect(0x4242);
        let syn = initiator.syn_packet(0);
        let listener = Connection::accept_syn(&syn.header);
        let answer = listener.state_packet(1);
        initiator
            .receive(&answer.encode(), 1_000_002)
            .expect("handshake");
        (initiator, listener)
    }

    /// A connected pair with the listener's answering state, for send-side
    /// pacing specs.
    fn handshake() -> Connection {
        let mut initiator = Connection::connect(0x1717);
        let syn = initiator.syn_packet(0);
        let listener = Connection::accept_syn(&syn.header);
        let mut answer = listener.state_packet(1);
        // The listener advertises a 4 KiB receive window.
        answer.header.window_size = 4 * 1024;
        initiator.receive(&answer.encode(), 2).expect("handshake");
        initiator
    }

    /// The next sequence number the receiver would accept in order.
    fn receiver_window_edge(receiver: &Connection) -> u16 {
        receiver.their_seq_hint()
    }

    /// Encodes a bare header into a minimal `ST_DATA` packet for specs.
    fn wire_packet(header: &Header, payload: &[u8]) -> Vec<u8> {
        Packet {
            header: *header,
            extensions: Vec::new(),
            payload: Cow::Borrowed(payload),
        }
        .encode()
    }
}
