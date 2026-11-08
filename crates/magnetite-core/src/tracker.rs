//! UDP tracker client (BEP 15) and compact peer-list parsing (BEP 23/7).
//!
//! The tracker is the first network edge of the stack, and it keeps the
//! house rules: parsing stays runtime-free and allocation-bounded (fixed
//! wire layouts sliced straight out of the received datagram), adversarial
//! bytes are typed errors — never panics — and the only I/O is [`std::net`],
//! so embedders can drive it from any executor or none at all.
//!
//! Protocol shape (BEP 15):
//!
//! - `connect`: a 16-byte request ([`PROTOCOL_ID`] + action 0 + transaction
//!   id) answered by a 16-byte reply carrying the 8-byte connection id.
//! - `announce`: a 98-byte request (connection id, action 1, transaction id,
//!   infohash, peer id, counters, event, ip, key, `num_want`, port) answered
//!   by a 20-byte header followed by 6-byte `IPv4:port` peer entries in the
//!   BEP 23 compact form; [`parse_compact_peers_v6`] handles the BEP 7
//!   18-byte `IPv6` spelling.
//! - `error`: action 3 with a UTF-8 message, surfaced as [`Error::Tracker`].

use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs as _};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// The magic constant every UDP tracker handshake starts with
/// (`0x41727101980`, big-endian).
pub const PROTOCOL_ID: i64 = 0x00_00_04_17_27_10_19_80;

/// Wire size of a connect request and of its reply.
pub const CONNECT_PACKET_LEN: usize = 16;

/// Wire size of an announce request (BEP 15 §announce request).
pub const ANNOUNCE_REQUEST_LEN: usize = 98;

/// Wire size of an announce reply header (interval + leechers + seeders).
pub const ANNOUNCE_RESPONSE_HEADER_LEN: usize = 20;

/// Upper bound for a single tracker datagram we are willing to buffer.
/// Real replies are far below this; the cap keeps a spoofed peer from
/// making us allocate without bound.
pub const MAX_RESPONSE_LEN: usize = 65_536;

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// Lifecycle event an announce reports (BEP 15 wire values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Event {
    /// Periodic update (wire `0`).
    #[default]
    None,
    /// The download just completed (wire `1`).
    Completed,
    /// First announce for this swarm membership (wire `2`).
    Started,
    /// Graceful leave (wire `3`).
    Stopped,
}

impl Event {
    /// The wire value this event encodes to.
    #[must_use]
    pub fn to_wire(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }
}

/// Parameters of a BEP 15 announce, in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceParams {
    /// Swarm this announce belongs to.
    pub info_hash: InfoHash,
    /// The announcing client's 20-byte peer id (`-qB4550-...` style).
    pub peer_id: [u8; 20],
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Bytes still missing.
    pub left: u64,
    /// Bytes uploaded so far.
    pub uploaded: u64,
    /// Lifecycle event this announce reports.
    pub event: Event,
    /// Client IP the tracker should advertise; `None` means the tracker
    /// uses the datagram's source address (wire `0`).
    pub ip: Option<Ipv4Addr>,
    /// Per-session key that lets a client prove identity across IP changes.
    pub key: u32,
    /// Maximum number of peers wanted; `-1` means the tracker's default.
    pub num_want: i32,
    /// The client's listen port.
    pub port: u16,
}

impl AnnounceParams {
    /// Announce parameters with the common defaults: zeroed counters, no
    /// event, no advertised IP, key `0`, and `num_want = -1`.
    #[must_use]
    pub fn new(info_hash: InfoHash, peer_id: [u8; 20], port: u16) -> Self {
        Self {
            info_hash,
            peer_id,
            downloaded: 0,
            left: 0,
            uploaded: 0,
            event: Event::None,
            ip: None,
            key: 0,
            num_want: -1,
            port,
        }
    }

    /// Encodes the announce request for `connection_id`/`transaction_id`
    /// into its fixed 98-byte wire layout.
    #[must_use]
    pub fn encode(&self, connection_id: i64, transaction_id: u32) -> [u8; ANNOUNCE_REQUEST_LEN] {
        let mut out = [0_u8; ANNOUNCE_REQUEST_LEN];
        out[0..8].copy_from_slice(&connection_id.to_be_bytes());
        out[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        out[12..16].copy_from_slice(&transaction_id.to_be_bytes());
        out[16..36].copy_from_slice(&self.info_hash.as_bytes());
        out[36..56].copy_from_slice(&self.peer_id);
        out[56..64].copy_from_slice(&self.downloaded.to_be_bytes());
        out[64..72].copy_from_slice(&self.left.to_be_bytes());
        out[72..80].copy_from_slice(&self.uploaded.to_be_bytes());
        out[80..84].copy_from_slice(&self.event.to_wire().to_be_bytes());
        let ip_bits = self.ip.map_or(0, Ipv4Addr::to_bits);
        out[84..88].copy_from_slice(&ip_bits.to_be_bytes());
        out[88..92].copy_from_slice(&self.key.to_be_bytes());
        out[92..96].copy_from_slice(&self.num_want.to_be_bytes());
        out[96..98].copy_from_slice(&self.port.to_be_bytes());
        out
    }
}

/// Builds the 16-byte connect request that starts a UDP tracker handshake.
#[must_use]
pub fn connect_request(transaction_id: u32) -> [u8; CONNECT_PACKET_LEN] {
    let mut out = [0_u8; CONNECT_PACKET_LEN];
    out[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    out[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    out[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    out
}

/// Parses the 16-byte connect reply and returns the connection id.
///
/// # Errors
/// Returns [`Error::Tracker`] when the datagram is short, carries trailing
/// bytes, has a mismatched transaction id, has an unexpected action, or is
/// a tracker error reply (action 3).
pub fn parse_connect_response(transaction_id: u32, packet: &[u8]) -> Result<i64> {
    let body = split_response_header(ACTION_CONNECT, transaction_id, packet)?;
    if body.len() < 8 {
        return Err(Error::Tracker(format!(
            "connect response needs {CONNECT_PACKET_LEN} bytes, got {}",
            packet.len()
        )));
    }
    if body.len() > 8 {
        return Err(Error::Tracker(format!(
            "trailing bytes after connect response ({} extra)",
            body.len() - 8
        )));
    }
    Ok(i64::from_be_bytes([
        body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
    ]))
}

/// A parsed BEP 15 announce reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceResponse {
    /// Seconds the client should wait before re-announcing.
    pub interval: u32,
    /// Tracker's count of leechers in the swarm.
    pub leechers: u32,
    /// Tracker's count of seeders in the swarm.
    pub seeders: u32,
    /// Compact `IPv4` peer addresses, in tracker order.
    pub peers: Vec<SocketAddrV4>,
}

/// Parses a BEP 15 announce reply: 20 header bytes plus compact peers.
///
/// # Errors
/// Returns [`Error::Tracker`] when the header is short or mismatched, or
/// when the trailing peer bytes do not chunk into 6-byte entries.
pub fn parse_announce_response(transaction_id: u32, packet: &[u8]) -> Result<AnnounceResponse> {
    let body = split_response_header(ACTION_ANNOUNCE, transaction_id, packet)?;
    if body.len() < 12 {
        return Err(Error::Tracker(format!(
            "announce response needs at least {ANNOUNCE_RESPONSE_HEADER_LEN} bytes, got {}",
            packet.len()
        )));
    }
    Ok(AnnounceResponse {
        interval: read_u32(body, 0),
        leechers: read_u32(body, 4),
        seeders: read_u32(body, 8),
        peers: parse_compact_peers_v4(&body[12..])?,
    })
}

/// Splits a BEP 23 compact `IPv4` peer list into socket addresses.
///
/// # Errors
/// Returns [`Error::Tracker`] when the length is not a multiple of 6.
pub fn parse_compact_peers_v4(packet: &[u8]) -> Result<Vec<SocketAddrV4>> {
    if packet.len() % 6 != 0 {
        return Err(Error::Tracker(format!(
            "compact peer list length {} is not a multiple of 6",
            packet.len()
        )));
    }
    Ok(packet
        .chunks_exact(6)
        .map(|entry| {
            SocketAddrV4::new(
                Ipv4Addr::new(entry[0], entry[1], entry[2], entry[3]),
                u16::from_be_bytes([entry[4], entry[5]]),
            )
        })
        .collect())
}

/// Splits a BEP 7 compact `IPv6` peer list (18-byte entries) into addresses.
///
/// # Errors
/// Returns [`Error::Tracker`] when the length is not a multiple of 18.
pub fn parse_compact_peers_v6(packet: &[u8]) -> Result<Vec<SocketAddrV6>> {
    if packet.len() % 18 != 0 {
        return Err(Error::Tracker(format!(
            "compact peer list length {} is not a multiple of 18",
            packet.len()
        )));
    }
    Ok(packet
        .chunks_exact(18)
        .map(|entry| {
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&entry[0..16]);
            SocketAddrV6::new(
                Ipv6Addr::from(octets),
                u16::from_be_bytes([entry[16], entry[17]]),
                0,
                0,
            )
        })
        .collect())
}

/// Resolves a `udp://host:port[/path]` tracker URL to a socket address.
///
/// The announce path part is accepted and ignored: the UDP protocol does
/// not use it, but real-world announce URLs always carry one.
///
/// # Errors
/// Returns [`Error::Tracker`] when the scheme is missing, the host:port is
/// empty, or the host cannot be resolved.
pub fn parse_udp_tracker_url(url: &str) -> Result<SocketAddr> {
    let rest = url
        .strip_prefix("udp://")
        .ok_or_else(|| Error::Tracker("tracker URL must start with `udp://`".into()))?;
    let host_port = rest.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return Err(Error::Tracker("tracker URL has no host:port".into()));
    }
    host_port
        .to_socket_addrs()
        .map_err(|err| Error::Tracker(format!("cannot resolve {host_port:?}: {err}")))?
        .next()
        .ok_or_else(|| Error::Tracker(format!("cannot resolve {host_port:?} to a socket address")))
}

/// A blocking UDP tracker connection: `connect` once, announce repeatedly.
///
/// The socket is connected (source-validated by the OS), read/write timeouts
/// cover both directions, and the negotiated connection id is reused across
/// announces per BEP 15.
#[derive(Debug)]
pub struct UdpTrackerClient {
    socket: std::net::UdpSocket,
    server: SocketAddr,
    connection_id: i64,
    timeout: Duration,
}

impl UdpTrackerClient {
    /// Handshakes with `tracker_url` (`udp://host:port[/path]`) and returns
    /// a client bound to an ephemeral local port.
    ///
    /// # Errors
    /// Returns [`Error::Tracker`] for URL/resolution problems and tracker
    /// error replies, and [`Error::Io`] for socket failures.
    pub fn connect(tracker_url: &str, timeout: Duration) -> Result<Self> {
        let server = parse_udp_tracker_url(tracker_url)?;
        let bind_addr = if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = std::net::UdpSocket::bind(bind_addr)?;
        socket.connect(server)?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;

        let transaction_id = next_transaction_id();
        socket.send(&connect_request(transaction_id))?;
        let reply = recv_reply(&socket, timeout)?;
        let connection_id = parse_connect_response(transaction_id, &reply)?;

        Ok(Self {
            socket,
            server,
            connection_id,
            timeout,
        })
    }

    /// Announces `params` on the established connection.
    ///
    /// # Errors
    /// Returns [`Error::Tracker`] for malformed or error replies and
    /// timeouts, and [`Error::Io`] for socket failures.
    pub fn announce(&self, params: &AnnounceParams) -> Result<AnnounceResponse> {
        let transaction_id = next_transaction_id();
        let request = params.encode(self.connection_id, transaction_id);
        self.socket.send(&request)?;
        let reply = recv_reply(&self.socket, self.timeout)?;
        parse_announce_response(transaction_id, &reply)
    }

    /// The connection id negotiated during the handshake.
    #[must_use]
    pub fn connection_id(&self) -> i64 {
        self.connection_id
    }

    /// The resolved tracker endpoint this client talks to.
    #[must_use]
    pub fn server_addr(&self) -> SocketAddr {
        self.server
    }
}

/// Generates a process-unique-enough transaction id: a time salt mixed
/// with a monotonic counter, per BEP 15's "random enough to match replies"
/// requirement without pulling in an RNG dependency.
fn next_transaction_id() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let salt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    salt ^ COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Sends `packet`, waits for one reply datagram within `timeout`, and maps
/// the timeout to [`Error::Tracker`] so callers see one error taxonomy.
fn recv_reply(socket: &std::net::UdpSocket, timeout: Duration) -> Result<Vec<u8>> {
    let mut buf = vec![0_u8; MAX_RESPONSE_LEN];
    match socket.recv(&mut buf) {
        Ok(n) => Ok(buf[..n].to_vec()),
        Err(err) if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut => {
            Err(Error::Tracker(format!("no reply within {timeout:?}")))
        }
        Err(err) => Err(Error::from(err)),
    }
}

/// Shared response header check: length floor, action dispatch (error
/// replies are surfaced with their message), and transaction id equality.
fn split_response_header(
    expected_action: u32,
    expected_transaction_id: u32,
    packet: &[u8],
) -> Result<&[u8]> {
    if packet.len() < 8 {
        return Err(Error::Tracker(format!(
            "response needs at least 8 bytes, got {}",
            packet.len()
        )));
    }
    let action = read_u32(packet, 0);
    if action == ACTION_ERROR {
        return Err(tracker_error_from_body(packet));
    }
    if action != expected_action {
        return Err(Error::Tracker(format!(
            "unexpected action {action} (expected {expected_action})"
        )));
    }
    let transaction_id = read_u32(packet, 4);
    if transaction_id != expected_transaction_id {
        return Err(Error::Tracker(format!(
            "transaction id mismatch: expected {expected_transaction_id:#010x}, got {transaction_id:#010x}"
        )));
    }
    Ok(&packet[8..])
}

/// Renders an action-3 reply as [`Error::Tracker`] with its UTF-8 message.
fn tracker_error_from_body(packet: &[u8]) -> Error {
    let message = String::from_utf8_lossy(&packet[8..]);
    if message.is_empty() {
        Error::Tracker("tracker replied with an empty error message".into())
    } else {
        Error::Tracker(format!("tracker replied: {message}"))
    }
}

/// Big-endian `u32` read at `offset`; callers bounds-check first.
fn read_u32(packet: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        packet[offset],
        packet[offset + 1],
        packet[offset + 2],
        packet[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    const TXID: u32 = 0x1234_5678;
    const CONN_ID: i64 = 0x1234_5678_9abc_def0;

    fn peer_id() -> [u8; 20] {
        *b"-qB4550-magnetite-x!"
    }

    fn sample_params() -> AnnounceParams {
        let mut params = AnnounceParams::new(InfoHash::from_bytes([0xAB; 20]), peer_id(), 6881);
        params.downloaded = 1024;
        params.left = 512;
        params.uploaded = 0;
        params.event = Event::Started;
        params.key = 7;
        params
    }

    /// Builds a well-formed announce reply around the given peer bytes.
    fn announce_reply(transaction_id: u32, peers: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        packet.extend_from_slice(&transaction_id.to_be_bytes());
        packet.extend_from_slice(&1_800_u32.to_be_bytes());
        packet.extend_from_slice(&3_u32.to_be_bytes());
        packet.extend_from_slice(&9_u32.to_be_bytes());
        packet.extend_from_slice(peers);
        packet
    }

    fn error_reply(transaction_id: u32, message: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        packet.extend_from_slice(&transaction_id.to_be_bytes());
        packet.extend_from_slice(message);
        packet
    }

    /// Spawns a UDP server on the loopback that answers every datagram by
    /// calling `responder` with the received packet.
    fn spawn_tracker<F>(responder: F) -> SocketAddr
    where
        F: Fn(&[u8]) -> Vec<u8> + Send + 'static,
    {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0_u8; 512];
            loop {
                let (n, peer) = socket.recv_from(&mut buf).unwrap();
                let reply = responder(&buf[..n]);
                socket.send_to(&reply, peer).unwrap();
            }
        });
        addr
    }

    fn request_fields(packet: &[u8]) -> (u32, u32) {
        (read_u32(packet, 8), read_u32(packet, 12))
    }

    #[test]
    fn event_wire_values_match_bep15() {
        assert_eq!(Event::None.to_wire(), 0);
        assert_eq!(Event::Completed.to_wire(), 1);
        assert_eq!(Event::Started.to_wire(), 2);
        assert_eq!(Event::Stopped.to_wire(), 3);
        assert_eq!(Event::default(), Event::None);
    }

    #[test]
    fn connect_request_pins_wire_layout() {
        let packet = connect_request(TXID);
        assert_eq!(packet.len(), CONNECT_PACKET_LEN);
        assert_eq!(packet.len(), 16);
        assert_eq!(&packet[0..8], &PROTOCOL_ID.to_be_bytes());
        assert_eq!(&packet[8..12], &0_u32.to_be_bytes());
        assert_eq!(&packet[12..16], &TXID.to_be_bytes());
    }

    #[test]
    fn connect_request_uses_the_documented_magic_constant() {
        assert_eq!(
            PROTOCOL_ID.to_be_bytes(),
            [0, 0, 0x04, 0x17, 0x27, 0x10, 0x19, 0x80]
        );
    }

    #[test]
    fn announce_request_pins_wire_layout() {
        let packet = sample_params().encode(CONN_ID, TXID);
        assert_eq!(packet.len(), ANNOUNCE_REQUEST_LEN);
        assert_eq!(packet.len(), 98);
        assert_eq!(packet[0..8], CONN_ID.to_be_bytes());
        assert_eq!(&packet[8..12], &1_u32.to_be_bytes());
        assert_eq!(&packet[12..16], &TXID.to_be_bytes());
        assert_eq!(&packet[16..36], &[0xAB; 20]);
        assert_eq!(&packet[36..56], &peer_id());
        assert_eq!(&packet[56..64], &1_024_u64.to_be_bytes());
        assert_eq!(&packet[64..72], &512_u64.to_be_bytes());
        assert_eq!(&packet[72..80], &0_u64.to_be_bytes());
        assert_eq!(&packet[80..84], &Event::Started.to_wire().to_be_bytes());
        assert_eq!(&packet[84..88], &[0; 4]);
        assert_eq!(&packet[88..92], &7_u32.to_be_bytes());
        assert_eq!(&packet[92..96], &(-1_i32).to_be_bytes());
        assert_eq!(&packet[96..98], &6_881_u16.to_be_bytes());
    }

    #[test]
    fn announce_request_encodes_advertised_ip() {
        let mut params = sample_params();
        params.ip = Some(Ipv4Addr::new(192, 0, 2, 7));
        params.event = Event::Stopped;
        params.num_want = 50;
        let packet = params.encode(CONN_ID, TXID);
        assert_eq!(
            &packet[84..88],
            &Ipv4Addr::new(192, 0, 2, 7).to_bits().to_be_bytes()
        );
        assert_eq!(&packet[80..84], &Event::Stopped.to_wire().to_be_bytes());
        assert_eq!(&packet[92..96], &50_i32.to_be_bytes());
    }

    #[test]
    fn new_applies_bep15_defaults() {
        let params = AnnounceParams::new(InfoHash::from_bytes([0; 20]), peer_id(), 6881);
        assert_eq!(params.downloaded, 0);
        assert_eq!(params.left, 0);
        assert_eq!(params.uploaded, 0);
        assert_eq!(params.event, Event::None);
        assert_eq!(params.ip, None);
        assert_eq!(params.key, 0);
        assert_eq!(params.num_want, -1);
        assert_eq!(params.port, 6881);
    }

    #[test]
    fn parses_connect_response() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        packet.extend_from_slice(&TXID.to_be_bytes());
        packet.extend_from_slice(&CONN_ID.to_be_bytes());
        assert_eq!(parse_connect_response(TXID, &packet).unwrap(), CONN_ID);
    }

    #[test]
    fn rejects_short_connect_response() {
        let packet = [0_u8; 15];
        let err = parse_connect_response(TXID, &packet).unwrap_err();
        assert!(matches!(err, Error::Tracker(_)), "got {err}");
    }

    #[test]
    fn rejects_trailing_bytes_after_connect_response() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        packet.extend_from_slice(&TXID.to_be_bytes());
        packet.extend_from_slice(&CONN_ID.to_be_bytes());
        packet.push(0);
        let err = parse_connect_response(TXID, &packet).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("trailing")),
            "got {err}"
        );
    }

    #[test]
    fn rejects_connect_response_with_wrong_transaction_id() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        packet.extend_from_slice(&0xDEAD_BEEF_u32.to_be_bytes());
        packet.extend_from_slice(&CONN_ID.to_be_bytes());
        let err = parse_connect_response(TXID, &packet).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("transaction id")),
            "got {err}"
        );
    }

    #[test]
    fn rejects_connect_response_with_unexpected_action() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&2_u32.to_be_bytes());
        packet.extend_from_slice(&TXID.to_be_bytes());
        packet.extend_from_slice(&CONN_ID.to_be_bytes());
        let err = parse_connect_response(TXID, &packet).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("unexpected action")),
            "got {err}"
        );
    }

    #[test]
    fn error_action_surfaces_tracker_message() {
        let err = parse_connect_response(TXID, &error_reply(TXID, b"swarm full")).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("swarm full")),
            "got {err}"
        );
    }

    #[test]
    fn empty_error_message_is_reported_verbatim() {
        let err = parse_announce_response(TXID, &error_reply(TXID, b"")).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("empty error message")),
            "got {err}"
        );
    }

    #[test]
    fn parses_announce_response_with_peers() {
        let peers = [10, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 2, 0x1a, 0xe2];
        let packet = announce_reply(TXID, &peers);
        let parsed = parse_announce_response(TXID, &packet).unwrap();
        assert_eq!(parsed.interval, 1_800);
        assert_eq!(parsed.leechers, 3);
        assert_eq!(parsed.seeders, 9);
        assert_eq!(
            parsed.peers,
            vec![
                SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 6_881),
                SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 6_882),
            ]
        );
    }

    #[test]
    fn announce_response_without_peers_is_valid() {
        let parsed = parse_announce_response(TXID, &announce_reply(TXID, &[])).unwrap();
        assert_eq!(parsed.peers, []);
    }

    #[test]
    fn rejects_announce_response_with_ragged_peer_bytes() {
        let packet = announce_reply(TXID, &[10, 0, 0, 1, 0x1a]);
        let err = parse_announce_response(TXID, &packet).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("multiple of 6")),
            "got {err}"
        );
    }

    #[test]
    fn rejects_announce_response_with_short_header() {
        let packet = announce_reply(TXID, &[])[..19].to_vec();
        let err = parse_announce_response(TXID, &packet).unwrap_err();
        assert!(matches!(err, Error::Tracker(_)), "got {err}");
    }

    #[test]
    fn rejects_announce_response_with_wrong_transaction_id() {
        let packet = announce_reply(0xDEAD_BEEF, &[]);
        let err = parse_announce_response(TXID, &packet).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("transaction id")),
            "got {err}"
        );
    }

    #[test]
    fn parses_compact_peers_v4() {
        let bytes = [192, 0, 2, 1, 0x1f, 0x90, 192, 0, 2, 2, 0, 0];
        assert_eq!(
            parse_compact_peers_v4(&bytes).unwrap(),
            vec![
                SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 8_080),
                SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 0),
            ]
        );
    }

    #[test]
    fn empty_compact_peer_list_is_valid() {
        assert_eq!(parse_compact_peers_v4(&[]).unwrap(), []);
        assert_eq!(parse_compact_peers_v6(&[]).unwrap(), []);
    }

    #[test]
    fn rejects_ragged_compact_peer_lists() {
        assert!(parse_compact_peers_v4(&[0; 5]).is_err());
        assert!(parse_compact_peers_v6(&[0; 17]).is_err());
    }

    #[test]
    fn parses_compact_peers_v6() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        bytes.extend_from_slice(&0x1f_90_u16.to_be_bytes());
        let parsed = parse_compact_peers_v6(&bytes).unwrap();
        assert_eq!(
            parsed,
            vec![SocketAddrV6::new(
                Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
                8_080,
                0,
                0
            )]
        );
    }

    #[test]
    fn parses_udp_tracker_urls() {
        assert_eq!(
            parse_udp_tracker_url("udp://127.0.0.1:1337").unwrap(),
            "127.0.0.1:1337".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_udp_tracker_url("udp://127.0.0.1:1337/announce").unwrap(),
            "127.0.0.1:1337".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn rejects_malformed_udp_tracker_urls() {
        assert!(parse_udp_tracker_url("http://t:80").is_err());
        assert!(parse_udp_tracker_url("udp://").is_err());
        assert!(parse_udp_tracker_url("udp://no-port-here").is_err());
    }

    #[test]
    fn transaction_ids_do_not_repeat() {
        let first = next_transaction_id();
        let mut seen = std::collections::HashSet::new();
        seen.insert(first);
        for _ in 0..1_000 {
            assert!(seen.insert(next_transaction_id()));
        }
    }

    #[test]
    fn loopback_handshake_and_announce_round_trip() {
        let server = spawn_tracker(|packet| {
            let (action, txid) = request_fields(packet);
            match action {
                0 => {
                    let mut reply = Vec::new();
                    reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
                    reply.extend_from_slice(&txid.to_be_bytes());
                    reply.extend_from_slice(&CONN_ID.to_be_bytes());
                    reply
                }
                1 => announce_reply(txid, &[10, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 2, 0x1a, 0xe2]),
                _ => Vec::new(),
            }
        });

        let client =
            UdpTrackerClient::connect(&format!("udp://{server}/announce"), Duration::from_secs(2))
                .unwrap();
        assert_eq!(client.connection_id(), CONN_ID);
        assert_eq!(client.server_addr(), server);

        let response = client.announce(&sample_params()).unwrap();
        assert_eq!(response.interval, 1_800);
        assert_eq!(response.leechers, 3);
        assert_eq!(response.seeders, 9);
        assert_eq!(response.peers.len(), 2);
        assert_eq!(
            response.peers[0],
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 6_881)
        );
    }

    #[test]
    fn loopback_tracker_error_becomes_typed_error() {
        // The handshake succeeds; every announce gets an action-3 reply.
        let server = spawn_tracker(|packet| {
            let (action, txid) = request_fields(packet);
            if action == 0 {
                let mut reply = Vec::new();
                reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
                reply.extend_from_slice(&txid.to_be_bytes());
                reply.extend_from_slice(&CONN_ID.to_be_bytes());
                reply
            } else {
                error_reply(txid, b"swarm full")
            }
        });
        let client =
            UdpTrackerClient::connect(&format!("udp://{server}"), Duration::from_secs(2)).unwrap();
        let err = client.announce(&sample_params()).unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("swarm full")),
            "got {err}"
        );
    }

    #[test]
    fn silent_tracker_times_out_with_typed_error() {
        // Bind a socket but never answer: the client must give up within
        // its read timeout instead of hanging.
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = socket.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0_u8; 512];
            loop {
                let _ = socket.recv_from(&mut buf);
            }
        });
        let err = UdpTrackerClient::connect(&format!("udp://{server}"), Duration::from_millis(100))
            .unwrap_err();
        assert!(
            matches!(err, Error::Tracker(ref msg) if msg.contains("no reply")),
            "got {err}"
        );
    }
}
