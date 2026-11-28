//! Peer exchange: the BEP 10 extended handshake and the BEP 11 `ut_pex`
//! codec, plus the swarm ledger that complies with BEP 11's sender rules.
//!
//! `ARCHITECTURE.md` §9 pins the **M2** row this module lands the PEX leg
//! of:
//!
//! > **M2** | DHT (BEP 5), PEX, uTP (BEP 29) | `magnetite-dht` crate;
//! > session state machine behind `&self` + internal tasks.
//!
//! Peer exchange rides the peer wire extension seam: an `m` table entry in
//! the extended handshake (BEP 10) negotiates the `ut_pex` message id, and
//! every later extended frame carrying that id is a bencoded `ut_pex` dict
//! (BEP 11) of compact peers. The codec stays runtime-free, borrowed where
//! possible, and strict about shape: contact strings must tile in 6-byte
//! (IPv4) or 18-byte (IPv6) strides, flag strings must count one byte per
//! announced peer, and an address may not be both added and dropped within
//! one message. The receiver side is lenient about *which* keys appear —
//! unknown dict keys are ignored so extensions keep evolving, and an
//! all-empty dict parses as an empty exchange rather than an error.
//!
//! [`PexLedger`] is the sender-side bookkeeping BEP 11's rules imply: the
//! first message carries the full roster, later messages carry deltas, and
//! after the initial message the added and dropped lists cap at
//! [`MAX_ANNOUNCED_PEERS`] entries each. Batching cadence ("no more than 1
//! PEX message per minute") is wall-clock policy, so it stays with the
//! caller: nothing here knows a clock.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::bencode::{self, Value};
use crate::error::{Error, Result};

/// The `m` table name a client registers for peer exchange (BEP 11).
pub const PEX_NAME: &str = "ut_pex";

/// Combined cap on `added` entries and on `dropped` entries per message,
/// once the initial full-list message is out: "Except for the initial PEX
/// message the combined amount of added v4/v6 contacts should not exceed
/// 50 entries" (BEP 11). The same cap applies to dropped entries.
pub const MAX_ANNOUNCED_PEERS: usize = 50;

/// Length of one compact IPv4 contact: 4 address bytes + 2 port bytes
/// (the BEP 23 wire form `ut_pex` reuses).
pub const COMPACT_V4_LEN: usize = 6;

/// Length of one compact IPv6 contact: 16 address bytes + 2 port bytes
/// (the BEP 7 wire form `ut_pex` reuses).
pub const COMPACT_V6_LEN: usize = 18;

/// The one-byte flag string `ut_pex` attaches per announced peer (the
/// `added.f`/`added6.f` fields). Bits: `0x01` prefers encryption, `0x02`
/// seed/upload-only, `0x04` supports uTP, `0x08` `ut_holepunch`, `0x10`
/// outgoing connection. Every other bit is reserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PexFlags(pub u8);

impl PexFlags {
    /// `0x01` — the peer prefers encrypted connections.
    pub const PREFERS_ENCRYPTION: u8 = 0x01;
    /// `0x02` — the peer is a seed (upload-only).
    pub const SEED: u8 = 0x02;
    /// `0x04` — the peer also speaks uTP (BEP 29).
    pub const SUPPORTS_UTP: u8 = 0x04;
    /// `0x08` — the peer advertised `ut_holepunch` support.
    pub const UT_HOLEPUNCH: u8 = 0x08;
    /// `0x10` — the connection was outgoing and the peer is reachable.
    pub const OUTGOING: u8 = 0x10;

    /// The raw flag byte as it rides the wire.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// `0x01` — the peer prefers encrypted connections.
    #[must_use]
    pub const fn prefers_encryption(self) -> bool {
        self.0 & Self::PREFERS_ENCRYPTION != 0
    }

    /// `0x02` — the peer is a seed (upload-only).
    #[must_use]
    pub const fn seed(self) -> bool {
        self.0 & Self::SEED != 0
    }

    /// `0x04` — the peer also speaks uTP (BEP 29).
    #[must_use]
    pub const fn supports_utp(self) -> bool {
        self.0 & Self::SUPPORTS_UTP != 0
    }

    /// `0x08` — the peer advertised `ut_holepunch` support.
    #[must_use]
    pub const fn ut_holepunch(self) -> bool {
        self.0 & Self::UT_HOLEPUNCH != 0
    }

    /// `0x10` — the connection was outgoing and the peer is reachable.
    #[must_use]
    pub const fn outgoing(self) -> bool {
        self.0 & Self::OUTGOING != 0
    }
}

/// One announced peer: its address plus the flag byte that rode the
/// `added.f` (or `added6.f`) string next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PexPeer {
    /// The peer's address, IPv4 or IPv6.
    pub addr: SocketAddr,
    /// The flag byte announced beside it (all zero when the sender sent
    /// no flag string).
    pub flags: PexFlags,
}

/// The BEP 10 extended handshake: how a peer advertises its extensions.
///
/// Carried as extended message id `0`; every other extended id it lists in
/// its `m` table (including `ut_pex`'s) is negotiated here. Parsing is
/// strict about the shapes BEP 10 fixes and lenient about which optional
/// keys appear; unknown keys are ignored so the handshake keeps evolving.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtHandshake {
    /// The peer's client-and-version string (`v`), when advertised.
    pub client: Option<String>,
    /// The peer's TCP listen port (`p`), when advertised.
    pub listen_port: Option<u16>,
    /// The extension `m` table: extension name to negotiated message id.
    /// An id of `0` means "I no longer support this" and is preserved as
    /// parsed.
    pub extensions: BTreeMap<String, u8>,
    /// The peer's maximum outstanding requests (`reqq`), when advertised.
    pub max_requests: Option<u32>,
    /// The address the peer sees for *us* (`yourip`), when advertised.
    pub your_ip: Option<IpAddr>,
    /// The peer's own IPv6 address (`ipv6`), when advertised.
    pub ipv6: Option<Ipv6Addr>,
}

impl ExtHandshake {
    /// A handshake advertising `ut_pex` under the given negotiated
    /// extended message id — the shape both ends of a PEX-enabled session
    /// send.
    #[must_use]
    pub fn advertising_pex(ut_pex_id: u8) -> Self {
        let mut extensions = BTreeMap::new();
        extensions.insert(PEX_NAME.to_string(), ut_pex_id);
        Self {
            extensions,
            ..Self::default()
        }
    }

    /// The extended id the peer assigned to `ut_pex`, when it advertises
    /// PEX; `None` (or a `0` mapping) means the peer does not speak PEX.
    #[must_use]
    pub fn ut_pex_id(&self) -> Option<u8> {
        self.extensions.get(PEX_NAME).copied().filter(|id| *id != 0)
    }

    /// Parses a bencoded extended-handshake dict.
    ///
    /// # Errors
    /// Returns [`Error::Pex`] when the payload is not valid bencode or not
    /// a dict, an `m` entry's id is not a byte-range integer, `p`/`reqq`
    /// are out of range, `yourip` is neither 4 nor 16 bytes, `ipv6` is not
    /// 16 bytes, or the `v` client string is not UTF-8.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let value =
            bencode::decode(payload).map_err(|e| Error::Pex(format!("invalid bencode: {e}")))?;
        if value.as_dict().is_none() {
            return Err(Error::Pex(
                "the extended handshake must be a bencoded dict".into(),
            ));
        }

        let mut extensions = BTreeMap::new();
        if let Some(table) = value.get(b"m") {
            let table = table
                .as_dict()
                .ok_or_else(|| Error::Pex("the `m` extension table must be a dict".into()))?;
            for (name, id) in table {
                // Extension names are matched by exact string; a name that
                // is not UTF-8 cannot be matched by any implementation, so
                // it is skipped rather than fatal.
                let Ok(name) = std::str::from_utf8(name) else {
                    continue;
                };
                let id = id.as_int().ok_or_else(|| {
                    Error::Pex(format!("`m` entry {name:?} must carry an integer id"))
                })?;
                let id = u8::try_from(id).map_err(|_| {
                    Error::Pex(format!(
                        "`m` entry {name:?} id {id} is beyond the byte range"
                    ))
                })?;
                extensions.insert(name.to_string(), id);
            }
        }

        let listen_port = int_field(&value, b"p", "`p` listen port")?
            .map(|p| {
                u16::try_from(p)
                    .map_err(|_| Error::Pex(format!("`p` listen port {p} is out of port range")))
            })
            .transpose()?;
        let max_requests = int_field(&value, b"reqq", "`reqq` request cap")?
            .map(|r| {
                u32::try_from(r)
                    .map_err(|_| Error::Pex(format!("`reqq` request cap {r} is out of range")))
            })
            .transpose()?;
        let client = bytes_field(&value, b"v", "`v` client string")?
            .map(|v| {
                String::from_utf8(v.to_vec())
                    .map_err(|_| Error::Pex("`v` client string is not UTF-8".into()))
            })
            .transpose()?;
        let your_ip = bytes_field(&value, b"yourip", "`yourip` address")?
            .map(|wire| ip_from_compact(wire, "`yourip`"))
            .transpose()?;
        let ipv6 = bytes_field(&value, b"ipv6", "`ipv6` address")?
            .map(|wire| {
                let wire: [u8; 16] = wire.try_into().map_err(|_| {
                    Error::Pex(format!(
                        "`ipv6` is {} bytes; the address is 16 bytes",
                        wire.len()
                    ))
                })?;
                Ok::<Ipv6Addr, Error>(Ipv6Addr::from(wire))
            })
            .transpose()?;

        Ok(Self {
            client,
            listen_port,
            extensions,
            max_requests,
            your_ip,
            ipv6,
        })
    }

    /// Encodes the handshake into its bencoded wire dict. Empty optional
    /// fields are omitted; the `m` table is written whenever non-empty.
    ///
    /// Keys are emitted in bencode's canonical byte-wise sorted order, so
    /// `encode(parse(encode(h))) == encode(h)` for every handshake.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.push(b'd');
        if let Some(ipv6) = self.ipv6 {
            emit_bytes_key(&mut out, b"ipv6", &ipv6.octets());
        }
        if !self.extensions.is_empty() {
            out.extend_from_slice(b"1:m");
            out.push(b'd');
            for (name, id) in &self.extensions {
                emit_int_key(&mut out, name.as_bytes(), i64::from(*id));
            }
            out.push(b'e');
        }
        if let Some(port) = self.listen_port {
            emit_int_key(&mut out, b"p", i64::from(port));
        }
        if let Some(max) = self.max_requests {
            emit_int_key(&mut out, b"reqq", i64::from(max));
        }
        if let Some(client) = &self.client {
            emit_bytes_key(&mut out, b"v", client.as_bytes());
        }
        if let Some(ip) = self.your_ip {
            emit_bytes_key(&mut out, b"yourip", &compact_ip_bytes(ip));
        }
        out.push(b'e');
        out
    }
}

/// One `ut_pex` exchange (BEP 11): the peers the sender just connected to
/// (`added`, with per-peer flags) and the ones it disconnected from
/// (`dropped`). IPv4 and IPv6 contacts ride in separate fields — the wire
/// forms never mix families in one string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PexMessage {
    /// Newly connected IPv4 peers, in wire order.
    pub added: Vec<PexPeer>,
    /// Newly connected IPv6 peers, in wire order.
    pub added6: Vec<PexPeer>,
    /// Disconnected IPv4 peers, in wire order.
    pub dropped: Vec<SocketAddr>,
    /// Disconnected IPv6 peers, in wire order.
    pub dropped6: Vec<SocketAddr>,
}

impl PexMessage {
    /// Parses a bencoded `ut_pex` dict.
    ///
    /// Shape rules (strict, BEP 11): `added`/`dropped` contact strings
    /// tile in [`COMPACT_V4_LEN`]-byte strides, `added6`/`dropped6` in
    /// [`COMPACT_V6_LEN`]-byte strides, each `*.f` flag string counts one
    /// byte per announced peer, an address may not repeat within one
    /// list, and an address may not be both added and dropped in the same
    /// message. Unknown dict keys are ignored; an empty dict parses as an
    /// empty exchange (the sender's "at least one key" duty is theirs).
    ///
    /// # Errors
    /// Returns [`Error::Pex`] for any of the shape violations above or a
    /// non-dict / non-bencoded payload.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let value =
            bencode::decode(payload).map_err(|e| Error::Pex(format!("invalid bencode: {e}")))?;
        if value.as_dict().is_none() {
            return Err(Error::Pex(
                "the ut_pex message must be a bencoded dict".into(),
            ));
        }

        let added = value.get(b"added").and_then(Value::as_bytes);
        let flags_v4 = value.get(b"added.f").and_then(Value::as_bytes);
        let added6 = value.get(b"added6").and_then(Value::as_bytes);
        let flags_v6 = value.get(b"added6.f").and_then(Value::as_bytes);
        let dropped = value.get(b"dropped").and_then(Value::as_bytes);
        let dropped6 = value.get(b"dropped6").and_then(Value::as_bytes);

        let added = match added {
            Some(wire) => decode_v4_contacts(wire, "`added`")?,
            None => Vec::new(),
        };
        let flags = flags_or_empty(flags_v4, added.len())?;
        let added: Vec<PexPeer> = zip_flags(added, flags);

        let added6 = match added6 {
            Some(wire) => decode_v6_contacts(wire, "`added6`")?,
            None => Vec::new(),
        };
        let flags = flags_or_empty(flags_v6, added6.len())?;
        let added6: Vec<PexPeer> = zip_flags(added6, flags);

        let dropped = match dropped {
            Some(wire) => decode_v4_addrs(wire, "`dropped`")?,
            None => Vec::new(),
        };
        let dropped6 = match dropped6 {
            Some(wire) => decode_v6_addrs(wire, "`dropped6`")?,
            None => Vec::new(),
        };

        let message = Self {
            added,
            added6,
            dropped,
            dropped6,
        };
        check_no_duplicates(&message)?;
        Ok(message)
    }

    /// Encodes the exchange into its bencoded wire dict, omitting empty
    /// fields. The flag strings ride only when at least one announced
    /// peer carries a non-zero flag byte. Keys are emitted in bencode's
    /// canonical byte-wise sorted order, so re-encoding a parsed message
    /// is byte-identical.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(b'd');
        emit_peers(&mut out, b"added", b"added.f", &self.added);
        emit_peers(&mut out, b"added6", b"added6.f", &self.added6);
        if !self.dropped.is_empty() {
            emit_bytes_key(&mut out, b"dropped", &compact_v4_addrs(&self.dropped));
        }
        if !self.dropped6.is_empty() {
            emit_bytes_key(&mut out, b"dropped6", &compact_v6_addrs(&self.dropped6));
        }
        out.push(b'e');
        out
    }

    /// Whether the exchange announces nothing at all — the shape a caller
    /// skips sending (BEP 11 forbids empty `ut_pex` dicts on the wire).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.added6.is_empty()
            && self.dropped.is_empty()
            && self.dropped6.is_empty()
    }

    /// How many peers the exchange announces (IPv4 + IPv6 combined).
    #[must_use]
    pub fn added_count(&self) -> usize {
        self.added.len() + self.added6.len()
    }
}

/// The sender-side PEX ledger: turns the swarm's current roster into the
/// delta BEP 11 wants announced on this connection.
///
/// One ledger per outbound PEX-enabled connection. [`PexLedger::delta`]
/// diffs the caller's currently-connected roster against what this
/// connection already announced: the first message carries the full
/// roster, later ones carry only new adds and disconnects, and the same
/// address is never both added and dropped within one message (the diff's
/// disjoint halves make that shape impossible by construction). Dropped
/// entries beyond [`MAX_ANNOUNCED_PEERS`] are elided *and forgotten*
/// (they are gone regardless); added entries beyond the cap are elided
/// and retried on the next delta.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PexLedger {
    known: BTreeSet<SocketAddr>,
    initial_sent: bool,
}

impl PexLedger {
    /// A fresh ledger that has announced nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the next [`PexLedger::delta`] is the connection's initial
    /// full-roster message (exempt from the 50-entry cap). The initial
    /// status is spent by the first delta that composes a non-empty
    /// message — composing an empty roster keeps it unspent, because an
    /// empty message is never sent.
    #[must_use]
    pub const fn is_initial(&self) -> bool {
        !self.initial_sent
    }

    /// How many addresses this connection has announced and not yet
    /// dropped: what the remote peer currently believes about this
    /// session.
    #[must_use]
    pub fn known_count(&self) -> usize {
        self.known.len()
    }

    /// Composes the next announcement from the swarm's current roster.
    ///
    /// Duplicate roster entries (the same address twice) collapse to one,
    /// last flags winning — the ledger keys on address. The first
    /// non-empty delta announces the full roster; later deltas announce
    /// only adds and drops, capped at [`MAX_ANNOUNCED_PEERS`] entries
    /// each (BEP 11). Announced adds join the ledger, announced drops
    /// leave it; adds elided by the cap are still unknown, so the next
    /// delta retries them, while drops elided by the cap stay known and
    /// are dropped next time — the remote peer must eventually learn the
    /// truth about every address it was told.
    #[must_use]
    pub fn delta(&mut self, peers: impl IntoIterator<Item = PexPeer>) -> PexMessage {
        // Deduplicate by address; last flags win.
        let mut roster: BTreeMap<SocketAddr, PexFlags> = BTreeMap::new();
        for peer in peers {
            roster.insert(peer.addr, peer.flags);
        }

        let mut message = PexMessage::default();
        if self.initial_sent {
            // Adds: roster peers this connection has not announced yet.
            let mut added: Vec<PexPeer> = roster
                .iter()
                .filter(|(addr, _)| !self.known.contains(addr))
                .map(|(&addr, &flags)| PexPeer { addr, flags })
                .collect();
            added.truncate(MAX_ANNOUNCED_PEERS);
            // Drops: announced addresses that left the roster.
            let mut dropped: Vec<SocketAddr> = self
                .known
                .iter()
                .filter(|addr| !roster.contains_key(*addr))
                .copied()
                .collect();
            dropped.truncate(MAX_ANNOUNCED_PEERS);

            for peer in added {
                if peer.addr.is_ipv4() {
                    message.added.push(peer);
                } else {
                    message.added6.push(peer);
                }
            }
            for addr in dropped {
                if addr.is_ipv4() {
                    message.dropped.push(addr);
                } else {
                    message.dropped6.push(addr);
                }
            }
        } else {
            // The initial message carries the full roster.
            for (&addr, &flags) in &roster {
                let peer = PexPeer { addr, flags };
                if peer.addr.is_ipv4() {
                    message.added.push(peer);
                } else {
                    message.added6.push(peer);
                }
            }
        }

        // Bookkeeping follows what actually composed, so the remote peer's
        // view can converge to the truth: announced adds are known, and
        // announced drops are forgotten (elided adds retry next round,
        // elided drops stay pending).
        for peer in message.added.iter().chain(&message.added6) {
            self.known.insert(peer.addr);
        }
        for addr in message.dropped.iter().chain(&message.dropped6) {
            self.known.remove(addr);
        }
        if !message.is_empty() {
            self.initial_sent = true;
        }
        message
    }
}

/// Fetches an integer field, when present, with a typed shape error.
fn int_field(dict: &Value<'_>, key: &[u8], what: &str) -> Result<Option<i64>> {
    match dict.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_int()
            .map(Some)
            .ok_or_else(|| Error::Pex(format!("{what} must be an integer"))),
    }
}

/// Fetches a byte-string field, when present, with a typed shape error.
fn bytes_field<'a>(dict: &'a Value<'a>, key: &[u8], what: &str) -> Result<Option<&'a [u8]>> {
    match dict.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bytes()
            .map(Some)
            .ok_or_else(|| Error::Pex(format!("{what} must be a byte string"))),
    }
}

/// The compact address form `yourip` carries: 4 bytes for IPv4, 16 for
/// IPv6 (no port).
fn ip_from_compact(wire: &[u8], what: &str) -> Result<IpAddr> {
    match wire {
        [a, b, c, d] => Ok(IpAddr::V4(Ipv4Addr::new(*a, *b, *c, *d))),
        wire if wire.len() == 16 => {
            let octets: [u8; 16] = wire.try_into().expect("length checked above");
            Ok(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => Err(Error::Pex(format!(
            "{what} is {} bytes; the compact address form is 4 or 16",
            wire.len()
        ))),
    }
}

/// The compact address bytes `yourip` carries back to the peer.
fn compact_ip_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

/// Decodes a compact IPv4 contact string into peers with blank flags
/// (flag bytes, when present, ride in separately).
fn decode_v4_contacts(wire: &[u8], what: &str) -> Result<Vec<PexPeer>> {
    if wire.len() % COMPACT_V4_LEN != 0 {
        return Err(Error::Pex(format!(
            "{what} is {} bytes; compact IPv4 contacts tile in {COMPACT_V4_LEN}-byte strides",
            wire.len()
        )));
    }
    Ok(wire
        .chunks_exact(COMPACT_V4_LEN)
        .map(|contact| PexPeer {
            addr: SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(contact[0], contact[1], contact[2], contact[3]),
                u16::from_be_bytes([contact[4], contact[5]]),
            )),
            flags: PexFlags::default(),
        })
        .collect())
}

/// Decodes a compact IPv6 contact string into peers with blank flags.
fn decode_v6_contacts(wire: &[u8], what: &str) -> Result<Vec<PexPeer>> {
    if wire.len() % COMPACT_V6_LEN != 0 {
        return Err(Error::Pex(format!(
            "{what} is {} bytes; compact IPv6 contacts tile in {COMPACT_V6_LEN}-byte strides",
            wire.len()
        )));
    }
    Ok(wire
        .chunks_exact(COMPACT_V6_LEN)
        .map(|contact| PexPeer {
            addr: SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from({
                    let octets: [u8; 16] = contact[..16].try_into().expect("stride checked above");
                    octets
                }),
                u16::from_be_bytes([contact[16], contact[17]]),
                0,
                0,
            )),
            flags: PexFlags::default(),
        })
        .collect())
}

/// Decodes a compact IPv4 address string into dropped addresses.
fn decode_v4_addrs(wire: &[u8], what: &str) -> Result<Vec<SocketAddr>> {
    Ok(decode_v4_contacts(wire, what)?
        .into_iter()
        .map(|peer| peer.addr)
        .collect())
}

/// Decodes a compact IPv6 address string into dropped addresses.
fn decode_v6_addrs(wire: &[u8], what: &str) -> Result<Vec<SocketAddr>> {
    Ok(decode_v6_contacts(wire, what)?
        .into_iter()
        .map(|peer| peer.addr)
        .collect())
}

/// The per-peer flag string: optional on the wire, one byte per announced
/// peer when present.
fn flags_or_empty(flags: Option<&[u8]>, expected: usize) -> Result<Vec<PexFlags>> {
    match flags {
        None => Ok(vec![PexFlags::default(); expected]),
        Some(wire) => {
            if wire.len() != expected {
                return Err(Error::Pex(format!(
                    "the flag string is {} bytes for {expected} announced peers: one flag byte per peer",
                    wire.len()
                )));
            }
            Ok(wire.iter().map(|&byte| PexFlags(byte)).collect())
        }
    }
}

/// Zips the flag bytes onto the peers decoded from a contact string.
fn zip_flags(peers: Vec<PexPeer>, flags: Vec<PexFlags>) -> Vec<PexPeer> {
    peers
        .into_iter()
        .zip(flags)
        .map(|(mut peer, flags)| {
            peer.flags = flags;
            peer
        })
        .collect()
}

/// Rejects repeated addresses within one list and addresses that are both
/// added and dropped in the same message (BEP 11 sender duties enforced
/// on the receive side as shape violations).
fn check_no_duplicates(message: &PexMessage) -> Result<()> {
    let mut added_seen = BTreeSet::new();
    for peer in message.added.iter().chain(&message.added6) {
        if !added_seen.insert(peer.addr) {
            return Err(Error::Pex(format!(
                "{} is added twice in one message",
                peer.addr
            )));
        }
    }
    let mut dropped_seen = BTreeSet::new();
    for addr in message.dropped.iter().chain(&message.dropped6) {
        if !dropped_seen.insert(*addr) {
            return Err(Error::Pex(format!(
                "{addr} is dropped twice in one message"
            )));
        }
    }
    for addr in dropped_seen {
        if added_seen.contains(&addr) {
            return Err(Error::Pex(format!(
                "{addr} is both added and dropped in one message"
            )));
        }
    }
    Ok(())
}

/// Emits one bencoded byte string.
fn emit_bencoded_string(out: &mut Vec<u8>, string: &[u8]) {
    out.extend_from_slice(string.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(string);
}

/// Emits one bencoded dict entry whose value is a byte string.
fn emit_bytes_key(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    emit_bencoded_string(out, key);
    emit_bencoded_string(out, value);
}

/// Emits one bencoded dict entry whose value is an integer.
fn emit_int_key(out: &mut Vec<u8>, key: &[u8], value: i64) {
    emit_bencoded_string(out, key);
    out.extend_from_slice(format!("i{value}e").as_bytes());
}

/// Emits the `added`/`added6` key pair, with the flag string riding only
/// when at least one peer carries a non-zero flag byte.
fn emit_peers(out: &mut Vec<u8>, key: &[u8], flags_key: &[u8], peers: &[PexPeer]) {
    if peers.is_empty() {
        return;
    }
    let is_ipv4 = peers[0].addr.is_ipv4();
    let mut contacts = Vec::with_capacity(
        peers.len()
            * if is_ipv4 {
                COMPACT_V4_LEN
            } else {
                COMPACT_V6_LEN
            },
    );
    for peer in peers {
        match peer.addr {
            SocketAddr::V4(v4) => {
                contacts.extend_from_slice(&v4.ip().octets());
                contacts.extend_from_slice(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(v6) => {
                contacts.extend_from_slice(&v6.ip().octets());
                contacts.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
    }
    emit_bytes_key(out, key, &contacts);
    let flags: Vec<u8> = peers.iter().map(|peer| peer.flags.bits()).collect();
    if flags.iter().any(|&byte| byte != 0) {
        emit_bytes_key(out, flags_key, &flags);
    }
}

/// The compact IPv4 form of the dropped-address list.
fn compact_v4_addrs(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(addrs.len() * COMPACT_V4_LEN);
    for addr in addrs {
        if let SocketAddr::V4(v4) = addr {
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    out
}

/// The compact IPv6 form of the dropped-address list.
fn compact_v6_addrs(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(addrs.len() * COMPACT_V6_LEN);
    for addr in addrs {
        if let SocketAddr::V6(v6) = addr {
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};

    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
    }

    fn v6(bytes: [u8; 16], port: u16) -> SocketAddr {
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(bytes), port, 0, 0))
    }

    /// ARCHITECTURE.md §9 pins the acceptance criterion this module lands:
    /// the M2 row's "PEX" leg — a `ut_pex` exchange parses into its added
    /// and dropped peers and re-encodes byte-identically.
    #[test]
    fn ut_pex_round_trips_added_flags_and_dropped() {
        let message = PexMessage {
            added: vec![
                PexPeer {
                    addr: v4(10, 0, 0, 1, 6_881),
                    flags: PexFlags(PexFlags::SEED | PexFlags::SUPPORTS_UTP),
                },
                PexPeer {
                    addr: v4(10, 0, 0, 2, 6_882),
                    flags: PexFlags::default(),
                },
            ],
            added6: vec![PexPeer {
                addr: v6(
                    [0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                    51_442,
                ),
                flags: PexFlags(PexFlags::PREFERS_ENCRYPTION),
            }],
            dropped: vec![v4(10, 0, 0, 3, 6_883)],
            dropped6: vec![v6(
                [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                51_443,
            )],
        };
        let wire = message.encode();
        let parsed = PexMessage::parse(&wire).expect("shape is legal");
        assert_eq!(parsed, message);
        assert_eq!(parsed.added_count(), 3);
        assert!(!parsed.is_empty());
    }

    #[test]
    fn flag_bits_mean_what_bep_11_names_them() {
        let flags = PexFlags(
            PexFlags::PREFERS_ENCRYPTION
                | PexFlags::SEED
                | PexFlags::SUPPORTS_UTP
                | PexFlags::UT_HOLEPUNCH
                | PexFlags::OUTGOING,
        );
        assert_eq!(flags.bits(), 0b0001_1111);
        assert!(flags.prefers_encryption());
        assert!(flags.seed());
        assert!(flags.supports_utp());
        assert!(flags.ut_holepunch());
        assert!(flags.outgoing());
        assert!(!PexFlags::default().seed());
    }

    #[test]
    fn contact_strings_must_tile_in_compact_strides() {
        // 7 bytes: not a multiple of the 6-byte IPv4 contact.
        let wire = b"d5:added7:abcdefge";
        let err = PexMessage::parse(wire).expect_err("stride violation");
        assert!(err.to_string().contains("compact"), "got {err}");
        // 19 bytes: not a multiple of the 18-byte IPv6 contact.
        let wire = b"d6:added619:\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12e";
        let err = PexMessage::parse(wire).expect_err("stride violation");
        assert!(err.to_string().contains("compact"), "got {err}");
    }

    #[test]
    fn flag_strings_must_count_one_byte_per_peer() {
        // One added IPv4 peer but two flag bytes.
        let wire = b"d5:added6:\x01\x02\x03\x04\x0d\x1b7:added.f2:xye";
        let err = PexMessage::parse(wire).expect_err("flag count mismatch");
        assert!(err.to_string().contains("flag"), "got {err}");
    }

    #[test]
    fn duplicate_addresses_and_add_drop_conflicts_are_typed_errors() {
        // The same IPv4 address twice inside `added`.
        let wire = b"d5:added12:\x01\x02\x03\x04\x0d\x1b\x01\x02\x03\x04\x0d\x1be";
        let err = PexMessage::parse(wire).expect_err("duplicate added");
        assert!(err.to_string().contains("twice"), "got {err}");
        // Added and dropped in one message.
        let wire = b"d5:added6:\x01\x02\x03\x04\x0d\x1b7:dropped6:\x01\x02\x03\x04\x0d\x1be";
        let err = PexMessage::parse(wire).expect_err("add/drop conflict");
        assert!(err.to_string().contains("both"), "got {err}");
    }

    #[test]
    fn parsing_is_lenient_about_empty_and_unknown_fields() {
        // No recognized keys: an empty exchange, not a crash or error.
        let parsed = PexMessage::parse(b"de").expect("empty exchange");
        assert!(parsed.is_empty());
        // Unknown keys ride along without disturbing the shape.
        let parsed = PexMessage::parse(b"d9:something5:value5:added6:\x01\x02\x03\x04\x0d\x1be")
            .expect("unknown key ignored");
        assert_eq!(parsed.added.len(), 1);
        assert_eq!(parsed.added[0].flags.bits(), 0);
    }

    #[test]
    fn encoding_omits_empty_fields_and_all_zero_flag_strings() {
        let only_drops = PexMessage {
            dropped: vec![v4(10, 0, 0, 9, 1_234)],
            ..PexMessage::default()
        };
        let wire = only_drops.encode();
        assert_ne!(wire, Vec::<u8>::new());
        let decoded = bencode::decode(&wire).unwrap();
        assert!(decoded.get(b"dropped").is_some());
        assert!(decoded.get(b"added").is_none());
        assert!(decoded.get(b"added.f").is_none());

        // All-zero flags: the flag string carries no information, so it
        // stays off the wire; parse still reconstructs zero flags.
        let quiet = PexMessage {
            added: vec![PexPeer {
                addr: v4(127, 0, 0, 1, 80),
                flags: PexFlags::default(),
            }],
            ..PexMessage::default()
        };
        let parsed = PexMessage::parse(&quiet.encode()).expect("shape is legal");
        assert_eq!(parsed, quiet);
    }

    #[test]
    fn non_dict_payloads_are_typed_errors() {
        assert!(PexMessage::parse(b"i42e").is_err());
        assert!(ExtHandshake::parse(b"l1:e").is_err());
        assert!(PexMessage::parse(b"").is_err());
    }

    #[test]
    fn extended_handshake_round_trips_the_m_table_and_fields() {
        let handshake = ExtHandshake {
            client: Some("Magnetite 0.7.0".into()),
            listen_port: Some(51_413),
            extensions: BTreeMap::from([
                (PEX_NAME.to_string(), 2_u8),
                ("ut_metadata".to_string(), 3_u8),
            ]),
            max_requests: Some(250),
            your_ip: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
            ipv6: Some(Ipv6Addr::from([
                0x20_u8, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
            ])),
        };
        let wire = handshake.encode();
        let parsed = ExtHandshake::parse(&wire).expect("shape is legal");
        assert_eq!(parsed, handshake);
        assert_eq!(parsed.ut_pex_id(), Some(2));
        assert_eq!(parsed.client.as_deref(), Some("Magnetite 0.7.0"));
        assert_eq!(parsed.listen_port, Some(51_413));
        assert_eq!(parsed.max_requests, Some(250));
    }

    #[test]
    fn a_zero_ut_pex_mapping_means_no_pex() {
        let handshake = ExtHandshake {
            extensions: BTreeMap::from([(PEX_NAME.to_string(), 0_u8)]),
            ..ExtHandshake::default()
        };
        assert_eq!(handshake.ut_pex_id(), None);
        // And so does no `m` table at all.
        assert_eq!(ExtHandshake::default().ut_pex_id(), None);
    }

    #[test]
    fn extended_handshake_rejects_out_of_shape_values() {
        // An m-table id beyond the byte range.
        let wire = b"d1:md5:ut_pexi300eee";
        let err = ExtHandshake::parse(wire).expect_err("id out of byte range");
        assert!(err.to_string().contains("byte"), "got {err}");
        // `yourip` must be 4 or 16 bytes, no port.
        let wire = b"d6:yourip5:\x01\x02\x03\x04\x05e";
        let err = ExtHandshake::parse(wire).expect_err("yourip stride");
        assert!(err.to_string().contains("yourip"), "got {err}");
        // `ipv6` must be 16 bytes.
        let wire = b"d4:ipv615:\x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00e";
        assert!(ExtHandshake::parse(wire).is_err());
        // `p` must fit a u16 port.
        let wire = b"d1:pi70000ee";
        assert!(ExtHandshake::parse(wire).is_err());
    }

    #[test]
    fn the_first_delta_is_the_full_roster() {
        let roster = [
            PexPeer {
                addr: v4(10, 0, 0, 1, 6_881),
                flags: PexFlags::default(),
            },
            PexPeer {
                addr: v6([0xfd; 16], 51_442),
                flags: PexFlags(PexFlags::SEED),
            },
            PexPeer {
                addr: v4(10, 0, 0, 2, 6_882),
                flags: PexFlags::default(),
            },
        ];
        let mut ledger = PexLedger::new();
        assert!(ledger.is_initial());
        let message = ledger.delta(roster);
        assert_eq!(message.added.len(), 2);
        assert_eq!(message.added6.len(), 1);
        assert!(message.dropped.is_empty() && message.dropped6.is_empty());
        assert_eq!(message.added_count(), 3);
        assert!(!ledger.is_initial());
        assert_eq!(ledger.known_count(), 3);
    }

    #[test]
    fn later_deltas_announce_only_changes() {
        let mut ledger = PexLedger::new();
        let a = v4(10, 0, 0, 1, 6_881);
        let b = v4(10, 0, 0, 2, 6_882);
        let c = v4(10, 0, 0, 3, 6_883);
        let _ = ledger.delta([
            PexPeer {
                addr: a,
                flags: PexFlags::default(),
            },
            PexPeer {
                addr: b,
                flags: PexFlags::default(),
            },
        ]);

        // b disconnects, c connects: the delta names exactly the change.
        let message = ledger.delta([
            PexPeer {
                addr: a,
                flags: PexFlags::default(),
            },
            PexPeer {
                addr: c,
                flags: PexFlags::default(),
            },
        ]);
        assert_eq!(
            message.added,
            vec![PexPeer {
                addr: c,
                flags: PexFlags::default()
            }]
        );
        assert_eq!(message.dropped, vec![b]);
        assert_eq!(ledger.known_count(), 2);
    }

    #[test]
    fn deltas_cap_at_fifty_entries_and_elided_adds_retry() {
        let mut ledger = PexLedger::new();
        // A non-empty baseline spends the initial full-roster message.
        let baseline: Vec<PexPeer> = (1_u8..=5)
            .map(|n| PexPeer {
                addr: v4(198, 51, 100, n, 6_881),
                flags: PexFlags::default(),
            })
            .collect();
        let _ = ledger.delta(baseline.iter().copied());

        let burst: Vec<PexPeer> = (0_u16..60)
            .map(|n| PexPeer {
                addr: v4(192, 0, 2, u8::try_from(n).unwrap_or(255), 6_881 + n),
                flags: PexFlags::default(),
            })
            .collect();
        let mut roster = baseline.clone();
        roster.extend(burst.iter().copied());
        let message = ledger.delta(roster.iter().copied());
        assert_eq!(message.added.len(), MAX_ANNOUNCED_PEERS);
        assert_eq!(message.dropped, Vec::<SocketAddr>::new());
        // The ten elided adds are still unannounced, so the next delta
        // retries exactly them.
        let message = ledger.delta(roster.iter().copied());
        assert_eq!(message.added.len(), 10);
        assert_eq!(ledger.known_count(), 65);
    }

    #[test]
    fn duplicate_roster_entries_collapse_last_flags_winning() {
        let mut ledger = PexLedger::new();
        let message = ledger.delta([
            PexPeer {
                addr: v4(10, 0, 0, 1, 6_881),
                flags: PexFlags::default(),
            },
            PexPeer {
                addr: v4(10, 0, 0, 1, 6_881),
                flags: PexFlags(PexFlags::SUPPORTS_UTP),
            },
        ]);
        assert_eq!(message.added.len(), 1);
        assert!(message.added[0].flags.supports_utp());
    }

    #[test]
    fn an_empty_roster_keeps_the_initial_message_unspent() {
        let mut ledger = PexLedger::new();
        // Composing from an empty swarm announces nothing, so the initial
        // full-roster status stays unspent for when peers arrive.
        let empty = ledger.delta([]);
        assert!(empty.is_empty());
        assert!(ledger.is_initial());
        let one = ledger.delta([PexPeer {
            addr: v4(10, 0, 0, 1, 6_881),
            flags: PexFlags::default(),
        }]);
        assert_eq!(one.added.len(), 1);
        assert!(!ledger.is_initial());
    }
}
