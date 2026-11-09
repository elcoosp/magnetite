//! Peer wire protocol codec: the BEP 10 handshake and BEP 3 message framing.
//!
//! This is the second network edge of the stack, and it keeps the same house
//! rules as the tracker: parsing stays runtime-free and allocation-bounded
//! (wire layouts sliced straight out of the caller's buffer), adversarial
//! bytes are typed errors — never panics — and there is no I/O here at all,
//! so the runtime-free downloader ([`crate::downloader`]) and the session
//! pump ([`crate::session`]) consume this codec directly, and the future
//! `tokio` stream session stays a thin socket edge on top.
//!
//! Protocol shape:
//!
//! - Handshake (BEP 10): `pstrlen(1) = 19`, `"BitTorrent protocol"`, 8
//!   reserved bytes (kept verbatim for extension negotiation), 20-byte
//!   infohash, 20-byte peer id — 68 bytes total. [`Handshake::parse`] demands
//!   an exact-length buffer; [`Handshake::parse_prefix`] hands back the rest
//!   of a stream chunk.
//! - Messages (BEP 3): a 4-byte big-endian length prefix (excluding itself),
//!   then the message id byte, then the payload. A zero length prefix is the
//!   keep-alive. Ids 0–9 are the base protocol; unknown ids are preserved as
//!   [`Message::Unknown`] so extensions survive a round-trip (lenient about
//!   *which* ids appear, strict about how known ones are shaped — one
//!   deliberate exception: a [`Message::Bitfield`] payload is carried
//!   verbatim, because bit shape is the piece count's business, enforced
//!   downstream by [`crate::downloader::PieceField::from_bitfield`]).
//! - Every frame is bounded by [`MAX_MESSAGE_LEN`]: a hostile length prefix
//!   is rejected before any parsing happens.

use std::borrow::Cow;

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// Length of the protocol name string (`"BitTorrent protocol"`), as spelled
/// by the handshake's `pstrlen` byte.
pub const PROTOCOL_NAME_LEN: usize = 19;

/// The `pstrlen` byte of a BEP 10 handshake: the protocol name's length.
/// Pinned against [`PROTOCOL_NAME_LEN`] by `wire_constants_match_bep10_and_bep3`.
const PSTRLEN: u8 = 19;

/// The protocol name every BEP 10 handshake carries after `pstrlen`.
pub const PROTOCOL_NAME: [u8; PROTOCOL_NAME_LEN] = *b"BitTorrent protocol";

/// Wire size of a complete BEP 10 handshake:
/// 1 + 19 + 8 + 20 + 20 bytes.
pub const HANDSHAKE_LEN: usize = 1 + PROTOCOL_NAME_LEN + 8 + 20 + 20;

/// Upper bound for a single framed message on the wire, in bytes.
const MAX_MESSAGE_LEN_WIRE: u32 = 4_194_304;

/// Upper bound for a single framed message we are willing to parse.
///
/// Real payloads sit far below this — this workspace's request blocks are
/// [`crate::downloader::BLOCK_LEN`] (16 KiB), and bitfields scale with the
/// piece count — so the cap is generous headroom that still turns a spoofed
/// 4 GiB length prefix into a typed error instead of a parse of an
/// impossible frame.
pub const MAX_MESSAGE_LEN: usize = 4_194_304;

/// Base protocol message ids (BEP 3 §peer messages).
const MSG_CHOKE: u8 = 0;
const MSG_UNCHOKE: u8 = 1;
const MSG_INTERESTED: u8 = 2;
const MSG_NOT_INTERESTED: u8 = 3;
const MSG_HAVE: u8 = 4;
const MSG_BITFIELD: u8 = 5;
const MSG_REQUEST: u8 = 6;
const MSG_PIECE: u8 = 7;
const MSG_CANCEL: u8 = 8;
const MSG_PORT: u8 = 9;

/// The BEP 10 handshake that opens a peer wire connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    /// The 8 reserved bytes, kept verbatim: extension negotiation (DHT,
    /// Fast Extension, BEP 10 extensions) reads them off the received
    /// handshake.
    pub reserved: [u8; 8],
    /// The 20-byte infohash of the torrent this connection serves.
    pub info_hash: InfoHash,
    /// The announcing peer's 20-byte id (`-qB4550-...` style).
    pub peer_id: [u8; 20],
}

impl Handshake {
    /// A handshake with zeroed reserved bytes.
    #[must_use]
    pub fn new(info_hash: InfoHash, peer_id: [u8; 20]) -> Self {
        Self {
            reserved: [0_u8; 8],
            info_hash,
            peer_id,
        }
    }

    /// Encodes the handshake into its fixed 68-byte wire layout.
    #[must_use]
    pub fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0_u8; HANDSHAKE_LEN];
        out[0] = PSTRLEN;
        out[1..20].copy_from_slice(&PROTOCOL_NAME);
        out[20..28].copy_from_slice(&self.reserved);
        out[28..48].copy_from_slice(&self.info_hash.as_bytes());
        out[48..68].copy_from_slice(&self.peer_id);
        out
    }

    /// Parses exactly one handshake; trailing bytes are an error.
    ///
    /// # Errors
    /// Returns [`Error::Peer`] when the buffer is short, carries trailing
    /// bytes, or spells a different `pstrlen`/protocol name.
    pub fn parse(packet: &[u8]) -> Result<Self> {
        if packet.len() < HANDSHAKE_LEN {
            return Err(Error::Peer(format!(
                "handshake needs {HANDSHAKE_LEN} bytes, got {}",
                packet.len()
            )));
        }
        if packet.len() > HANDSHAKE_LEN {
            return Err(Error::Peer(format!(
                "trailing bytes after handshake ({} extra)",
                packet.len() - HANDSHAKE_LEN
            )));
        }
        let (handshake, _) = Self::parse_prefix(packet)?;
        Ok(handshake)
    }

    /// Parses a handshake out of a stream chunk and returns the unconsumed
    /// remainder, ready for the first framed message.
    ///
    /// # Errors
    /// Returns [`Error::Peer`] when the buffer is short or does not spell a
    /// BEP 10 handshake.
    pub fn parse_prefix(packet: &[u8]) -> Result<(Self, &[u8])> {
        if packet.len() < HANDSHAKE_LEN {
            return Err(Error::Peer(format!(
                "handshake needs {HANDSHAKE_LEN} bytes, got {}",
                packet.len()
            )));
        }
        if packet[0] != PSTRLEN {
            return Err(Error::Peer(format!(
                "handshake pstrlen is {}, expected {PSTRLEN}",
                packet[0]
            )));
        }
        if packet[1..20] != PROTOCOL_NAME[..] {
            return Err(Error::Peer(
                "not a peer handshake: protocol name mismatch".into(),
            ));
        }
        let handshake = Self {
            reserved: copy_array::<8>(&packet[20..28]),
            info_hash: InfoHash::from_bytes(copy_array::<20>(&packet[28..48])),
            peer_id: copy_array::<20>(&packet[48..68]),
        };
        Ok((handshake, &packet[HANDSHAKE_LEN..]))
    }
}

/// One framed peer wire message, borrowed from the input where possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    /// Zero length prefix; no id byte, no payload.
    KeepAlive,
    /// Id 0 — the sender will not answer requests for now.
    Choke,
    /// Id 1 — the sender answers requests again.
    Unchoke,
    /// Id 2 — the sender wants to download.
    Interested,
    /// Id 3 — the sender stopped wanting to download.
    NotInterested,
    /// Id 4 — the sender completed the piece with this index.
    Have(u32),
    /// Id 5 — the sender's piece bitmask; empty is legal for a fresh leecher.
    Bitfield(Cow<'a, [u8]>),
    /// Id 6 — block request (`index`, `begin`, `length`).
    Request {
        /// Piece index.
        index: u32,
        /// Byte offset inside the piece.
        begin: u32,
        /// Requested block length (16 KiB in practice).
        length: u32,
    },
    /// Id 7 — a block of piece data (`index`, `begin`, `block`).
    Piece {
        /// Piece index.
        index: u32,
        /// Byte offset inside the piece.
        begin: u32,
        /// The block bytes, borrowed from the input when parsed.
        block: Cow<'a, [u8]>,
    },
    /// Id 8 — withdraws a matching request.
    Cancel {
        /// Piece index.
        index: u32,
        /// Byte offset inside the piece.
        begin: u32,
        /// Cancelled block length.
        length: u32,
    },
    /// Id 9 — the sender's DHT listen port (BEP 5 seam).
    Port(u16),
    /// Any other id, preserved with its payload so extensions round-trip.
    Unknown {
        /// The unassigned (or extension) message id.
        id: u8,
        /// The payload bytes, borrowed from the input when parsed.
        payload: Cow<'a, [u8]>,
    },
}

impl Message<'_> {
    /// Appends the framed wire form (length prefix + id + payload) to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Message::KeepAlive => out.extend_from_slice(&[0, 0, 0, 0]),
            Message::Choke => push_frame(out, MSG_CHOKE, &[]),
            Message::Unchoke => push_frame(out, MSG_UNCHOKE, &[]),
            Message::Interested => push_frame(out, MSG_INTERESTED, &[]),
            Message::NotInterested => push_frame(out, MSG_NOT_INTERESTED, &[]),
            Message::Have(index) => push_frame(out, MSG_HAVE, &index.to_be_bytes()),
            Message::Bitfield(bits) => push_frame(out, MSG_BITFIELD, bits),
            Message::Request {
                index,
                begin,
                length,
            } => push_frame(out, MSG_REQUEST, &block_body(*index, *begin, *length)),
            Message::Piece {
                index,
                begin,
                block,
            } => {
                let mut payload = Vec::with_capacity(8 + block.len());
                payload.extend_from_slice(&index.to_be_bytes());
                payload.extend_from_slice(&begin.to_be_bytes());
                payload.extend_from_slice(block);
                push_frame(out, MSG_PIECE, &payload);
            }
            Message::Cancel {
                index,
                begin,
                length,
            } => push_frame(out, MSG_CANCEL, &block_body(*index, *begin, *length)),
            Message::Port(port) => push_frame(out, MSG_PORT, &port.to_be_bytes()),
            Message::Unknown { id, payload } => push_frame(out, *id, payload),
        }
    }

    /// The framed wire form (length prefix + id + payload).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// An owned copy that outlives the input buffer — the seam the future
    /// stream reader uses to hand messages across await points.
    #[must_use]
    pub fn into_owned(&self) -> Message<'static> {
        match self {
            Message::KeepAlive => Message::KeepAlive,
            Message::Choke => Message::Choke,
            Message::Unchoke => Message::Unchoke,
            Message::Interested => Message::Interested,
            Message::NotInterested => Message::NotInterested,
            Message::Have(index) => Message::Have(*index),
            Message::Bitfield(bits) => Message::Bitfield(Cow::Owned(bits.to_vec())),
            Message::Request {
                index,
                begin,
                length,
            } => Message::Request {
                index: *index,
                begin: *begin,
                length: *length,
            },
            Message::Piece {
                index,
                begin,
                block,
            } => Message::Piece {
                index: *index,
                begin: *begin,
                block: Cow::Owned(block.to_vec()),
            },
            Message::Cancel {
                index,
                begin,
                length,
            } => Message::Cancel {
                index: *index,
                begin: *begin,
                length: *length,
            },
            Message::Port(port) => Message::Port(*port),
            Message::Unknown { id, payload } => Message::Unknown {
                id: *id,
                payload: Cow::Owned(payload.to_vec()),
            },
        }
    }
}

/// Reads the first framed message from `bytes`.
///
/// Returns `Ok(None)` when `bytes` does not yet hold a complete frame — the
/// stream caller reads more and retries — and a typed error when the frame
/// is provably malformed (hostile length prefix, unknown shape).
///
/// # Errors
/// Returns [`Error::Peer`] when the length prefix exceeds
/// [`MAX_MESSAGE_LEN`] or a known message id carries a malformed payload.
pub fn next_message(bytes: &[u8]) -> Result<Option<(Message<'_>, usize)>> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let length = read_u32(bytes, 0);
    if length == 0 {
        return Ok(Some((Message::KeepAlive, 4)));
    }
    if length > MAX_MESSAGE_LEN_WIRE {
        return Err(Error::Peer(format!(
            "length prefix {length} exceeds the {MAX_MESSAGE_LEN} byte cap"
        )));
    }
    // Bounded by the cap above, so the widening cannot truncate.
    let total = 4 + length as usize;
    if total > bytes.len() {
        return Ok(None);
    }
    let id = bytes[4];
    let payload = &bytes[5..total];
    let message = match id {
        MSG_CHOKE if payload.is_empty() => Message::Choke,
        MSG_UNCHOKE if payload.is_empty() => Message::Unchoke,
        MSG_INTERESTED if payload.is_empty() => Message::Interested,
        MSG_NOT_INTERESTED if payload.is_empty() => Message::NotInterested,
        MSG_HAVE if payload.len() == 4 => Message::Have(read_u32(payload, 0)),
        MSG_BITFIELD => Message::Bitfield(Cow::Borrowed(payload)),
        MSG_REQUEST if payload.len() == 12 => Message::Request {
            index: read_u32(payload, 0),
            begin: read_u32(payload, 4),
            length: read_u32(payload, 8),
        },
        MSG_PIECE if payload.len() >= 8 => Message::Piece {
            index: read_u32(payload, 0),
            begin: read_u32(payload, 4),
            block: Cow::Borrowed(&payload[8..]),
        },
        MSG_CANCEL if payload.len() == 12 => Message::Cancel {
            index: read_u32(payload, 0),
            begin: read_u32(payload, 4),
            length: read_u32(payload, 8),
        },
        MSG_PORT if payload.len() == 2 => {
            Message::Port(u16::from_be_bytes([payload[0], payload[1]]))
        }
        MSG_CHOKE | MSG_UNCHOKE | MSG_INTERESTED | MSG_NOT_INTERESTED | MSG_HAVE | MSG_REQUEST
        | MSG_PIECE | MSG_CANCEL | MSG_PORT => {
            return Err(Error::Peer(format!(
                "message id {id} carries a malformed payload of {} bytes",
                payload.len()
            )));
        }
        other => Message::Unknown {
            id: other,
            payload: Cow::Borrowed(payload),
        },
    };
    Ok(Some((message, total)))
}

/// Parses exactly one framed message; trailing bytes are an error.
///
/// # Errors
/// Returns [`Error::Peer`] when the input is truncated or carries trailing
/// bytes after the first frame, or when [`next_message`] rejects the frame.
pub fn parse_message(bytes: &[u8]) -> Result<Message<'_>> {
    let Some((message, consumed)) = next_message(bytes)? else {
        return Err(Error::Peer(format!(
            "truncated message: {} bytes cannot hold one full frame",
            bytes.len()
        )));
    };
    if consumed != bytes.len() {
        return Err(Error::Peer(format!(
            "trailing bytes after message ({} extra)",
            bytes.len() - consumed
        )));
    }
    Ok(message)
}

/// Appends a length-prefixed frame for `id` and `payload`.
///
/// The wire length is a `u32` and every payload this crate parses is capped
/// at [`MAX_MESSAGE_LEN`]; the `unwrap_or` below only exists so a caller
/// hand-building a multi-GiB payload gets a (wrong but bounded) frame
/// instead of a panic or a silent truncation mid-encode.
fn push_frame(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    let length = u32::try_from(payload.len() + 1).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.push(id);
    out.extend_from_slice(payload);
}

/// The 12-byte request/cancel body: index, begin, length.
fn block_body(index: u32, begin: u32, length: u32) -> [u8; 12] {
    let mut body = [0_u8; 12];
    body[0..4].copy_from_slice(&index.to_be_bytes());
    body[4..8].copy_from_slice(&begin.to_be_bytes());
    body[8..12].copy_from_slice(&length.to_be_bytes());
    body
}

/// Big-endian `u32` read at `offset`; callers bounds-check first.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Copies a bounds-checked slice into a fixed-size array.
fn copy_array<const N: usize>(slice: &[u8]) -> [u8; N] {
    let mut array = [0_u8; N];
    array.copy_from_slice(slice);
    array
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infohash::InfoHash;

    const INFO_HASH: InfoHash = InfoHash::from_bytes([0xAB; 20]);
    const PEER_ID: [u8; 20] = *b"-qB4550-magnetite-x!";

    fn sample_handshake() -> Handshake {
        Handshake::new(INFO_HASH, PEER_ID)
    }

    fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        push_frame(&mut out, id, payload);
        out
    }

    #[test]
    fn wire_constants_match_bep10_and_bep3() {
        assert_eq!(PROTOCOL_NAME_LEN, 19);
        assert_eq!(usize::from(PSTRLEN), PROTOCOL_NAME_LEN);
        assert_eq!(PROTOCOL_NAME, *b"BitTorrent protocol");
        assert_eq!(HANDSHAKE_LEN, 68);
        assert_eq!(u32::try_from(MAX_MESSAGE_LEN), Ok(MAX_MESSAGE_LEN_WIRE));
    }

    #[test]
    fn handshake_pins_bep10_wire_layout() {
        let packet = sample_handshake().encode();
        assert_eq!(packet.len(), HANDSHAKE_LEN);
        assert_eq!(packet[0], 19);
        assert_eq!(&packet[1..20], b"BitTorrent protocol");
        assert_eq!(&packet[20..28], &[0_u8; 8]);
        assert_eq!(&packet[28..48], &[0xAB; 20]);
        assert_eq!(&packet[48..68], &PEER_ID);
    }

    #[test]
    fn handshake_round_trips_through_parse() {
        assert_eq!(
            Handshake::parse(&sample_handshake().encode()).unwrap(),
            sample_handshake()
        );
    }

    #[test]
    fn handshake_parse_rejects_wrong_protocol_name() {
        let mut packet = sample_handshake().encode();
        packet[5] = b'X';
        let err = Handshake::parse(&packet).unwrap_err();
        assert!(err.to_string().contains("protocol name mismatch"));
    }

    #[test]
    fn handshake_parse_rejects_bad_pstrlen() {
        let mut packet = sample_handshake().encode();
        packet[0] = 18;
        assert!(Handshake::parse(&packet).is_err());
    }

    #[test]
    fn handshake_parse_rejects_wrong_length() {
        assert!(Handshake::parse(&[0_u8; 67]).is_err());
        assert!(Handshake::parse(&[0_u8; 69]).is_err());
    }

    #[test]
    fn handshake_parse_prefix_returns_the_remaining_bytes() {
        let mut chunk = sample_handshake().encode().to_vec();
        chunk.extend_from_slice(&[0, 0, 0, 0]);
        let (handshake, rest) = Handshake::parse_prefix(&chunk).unwrap();
        assert_eq!(handshake, sample_handshake());
        assert_eq!(rest, &[0, 0, 0, 0]);
    }

    #[test]
    fn handshake_parse_prefix_rejects_short_input() {
        assert!(Handshake::parse_prefix(&[0_u8; 20]).is_err());
    }

    #[test]
    fn keep_alive_is_four_zero_bytes() {
        assert_eq!(Message::KeepAlive.encode(), [0, 0, 0, 0]);
        assert_eq!(parse_message(&[0, 0, 0, 0]).unwrap(), Message::KeepAlive);
    }

    #[test]
    fn state_messages_round_trip() {
        for message in [
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
        ] {
            let encoded = message.encode();
            assert_eq!(encoded.len(), 5);
            assert_eq!(parse_message(&encoded).unwrap(), message);
        }
    }

    #[test]
    fn state_messages_reject_non_empty_payloads() {
        let mut packet = frame(MSG_CHOKE, &[0x07]);
        packet[0..4].copy_from_slice(&2_u32.to_be_bytes());
        assert!(parse_message(&packet).is_err());
    }

    #[test]
    fn have_round_trips() {
        let encoded = Message::Have(7).encode();
        assert_eq!(encoded, [0, 0, 0, 5, MSG_HAVE, 0, 0, 0, 7]);
        assert_eq!(parse_message(&encoded).unwrap(), Message::Have(7));
    }

    #[test]
    fn have_rejects_wrong_payload_size() {
        let packet = frame(MSG_HAVE, &[1, 2, 3]);
        assert!(parse_message(&packet).is_err());
    }

    #[test]
    fn request_and_cancel_round_trip() {
        let request = Message::Request {
            index: 9,
            begin: 65_536,
            length: 16_384,
        };
        let encoded = request.encode();
        assert_eq!(encoded.len(), 17);
        assert_eq!(parse_message(&encoded).unwrap(), request);

        let cancel = Message::Cancel {
            index: 9,
            begin: 65_536,
            length: 16_384,
        };
        assert_eq!(parse_message(&cancel.encode()).unwrap(), cancel);
    }

    #[test]
    fn request_rejects_wrong_payload_size() {
        let packet = frame(MSG_REQUEST, &[0, 1, 2]);
        assert!(parse_message(&packet).is_err());
    }

    #[test]
    fn piece_round_trips_and_stays_borrowed() {
        let block = [0x5A_u8; 16_384];
        let message = Message::Piece {
            index: 1,
            begin: 0,
            block: Cow::Borrowed(&block[..]),
        };
        let encoded = message.encode();
        assert_eq!(encoded.len(), 4 + 1 + 8 + 16_384);
        let parsed = parse_message(&encoded).unwrap();
        assert_eq!(parsed, message);
        assert!(matches!(
            parsed,
            Message::Piece {
                block: Cow::Borrowed(_),
                ..
            }
        ));
    }

    #[test]
    fn piece_rejects_short_payload() {
        let packet = frame(MSG_PIECE, &[0, 0, 0]);
        assert!(parse_message(&packet).is_err());
    }

    #[test]
    fn bitfield_round_trips_including_empty() {
        let message = Message::Bitfield(Cow::Borrowed(&[0b1100_0000, 0x00][..]));
        assert_eq!(parse_message(&message.encode()).unwrap(), message);
        let empty = Message::Bitfield(Cow::Borrowed(&[][..]));
        assert_eq!(parse_message(&empty.encode()).unwrap(), empty);
    }

    #[test]
    fn port_round_trips() {
        let encoded = Message::Port(6_881).encode();
        assert_eq!(encoded, [0, 0, 0, 3, MSG_PORT, 0x1A, 0xE1]);
        assert_eq!(parse_message(&encoded).unwrap(), Message::Port(6_881));
    }

    #[test]
    fn unknown_ids_are_preserved_for_round_tripping() {
        let wire = frame(27, &[1, 2, 3]);
        let parsed = parse_message(&wire).unwrap();
        assert_eq!(
            parsed,
            Message::Unknown {
                id: 27,
                payload: Cow::Borrowed(&[1, 2, 3][..])
            }
        );
        assert_eq!(parsed.encode(), wire);
    }

    #[test]
    fn into_owned_detaches_from_the_input_buffer() {
        let wire = frame(MSG_PIECE, &[0, 0, 0, 1, 0, 0, 0, 0, 7, 8, 9]);
        let parsed = parse_message(&wire).unwrap();
        let owned = parsed.into_owned();
        drop(wire);
        assert_eq!(
            owned,
            Message::Piece {
                index: 1,
                begin: 0,
                block: Cow::Owned(vec![7, 8, 9])
            }
        );
    }

    #[test]
    fn next_message_reports_incomplete_frames() {
        assert_eq!(next_message(&[0, 0]).unwrap(), None);
        let mut partial = frame(MSG_BITFIELD, &[0_u8; 20]);
        partial.truncate(4 + 10);
        assert_eq!(next_message(&partial).unwrap(), None);
        assert_eq!(next_message(&[]).unwrap(), None);
    }

    #[test]
    fn next_message_rejects_hostile_length_prefixes() {
        assert!(next_message(&[0xFF, 0xFF, 0xFF, 0xFF]).is_err());
        // 16 MiB prefix: above the cap, rejected before any slicing.
        assert!(next_message(&[0x01, 0x00, 0x00, 0x00]).is_err());
    }

    #[test]
    fn next_message_reports_consumed_bytes_for_consecutive_frames() {
        let mut wire = Message::KeepAlive.encode();
        wire.extend_from_slice(&Message::Have(3).encode());
        let (first, consumed) = next_message(&wire).unwrap().unwrap();
        assert_eq!(first, Message::KeepAlive);
        assert_eq!(consumed, 4);
        let (second, consumed) = next_message(&wire[consumed..]).unwrap().unwrap();
        assert_eq!(second, Message::Have(3));
        assert_eq!(consumed, 9);
    }

    #[test]
    fn parse_message_rejects_trailing_bytes() {
        let mut wire = Message::Have(1).encode();
        wire.push(0);
        assert!(parse_message(&wire).is_err());
    }
}
