//! The blocking UDP DHT node: client transactions plus a one-datagram
//! server, over plain [`std::net`].
//!
//! This is the I/O edge of the DHT groundwork, and it keeps the tracker
//! client's shape: a UDP socket bound on an ephemeral port, per-call read
//! timeouts, transaction ids matched against replies (stray datagrams are
//! ignored, never fatal), and typed errors for every failure. Unlike the
//! tracker there are *two* halves on one socket — a node both asks and
//! answers — so the server half is a one-datagram [`DhtNode::serve_once`]
//! the caller loops on, single-threaded and runtime-free. The async engine
//! (the plan's "session state machine behind `&self` + internal tasks") is
//! the next DHT drop; this node is the testable substrate it will pump.
//!
//! Serving a query does what a BEP 5 node does with the routing table:
//! every answered query first records the sender as a contact, `find_node`
//! replies with the table's closest contacts, `get_peers` issues a
//! rotating announce token and answers with stored peers (falling back to
//! closer contacts when the swarm is empty), and `announce_peer` honors
//! only a token this node issued. Unknown methods get KRPC error 204;
//! malformed datagrams are dropped as data, never crashes.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use magnetite_core::error::{Error, Result};

use crate::contact::Contact;
use crate::krpc::{
    self, METHOD_UNKNOWN_ERROR, Message, Method, PROTOCOL_ERROR, QueryArgs, Response,
    next_transaction_id,
};
use crate::node_id::NodeId;
use crate::store::{PeerStore, TokenIssuer};
use crate::table::{KBUCKET_SIZE, RoutingTable};

/// Wire size of a receive buffer: KRPC datagrams ride UDP, so nothing
/// beyond one IP datagram can arrive.
const MAX_DATAGRAM_LEN: usize = 65_536;

/// What one served datagram turned into, for callers that log, test, or
/// drive swarm logic off the answers their node gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedEvent {
    /// A `ping` was answered; the sender is now a table contact.
    Pinged {
        /// The sender's contact (id from the query, address from the
        /// datagram source).
        contact: Contact,
    },
    /// A `find_node` was answered with `returned` contacts.
    NeighborsServed {
        /// The sender's contact.
        contact: Contact,
        /// How many contacts the reply carried.
        returned: usize,
    },
    /// A `get_peers` was answered with a fresh token and `peers_returned`
    /// stored endpoints.
    PeersServed {
        /// The sender's contact.
        contact: Contact,
        /// The swarm that was asked about.
        info_hash: [u8; 20],
        /// How many `values`/`values6` entries the reply carried.
        peers_returned: usize,
    },
    /// An `announce_peer` was accepted and recorded.
    AnnounceAccepted {
        /// The sender's contact.
        contact: Contact,
        /// The swarm that was announced.
        info_hash: [u8; 20],
        /// The peer port that was stored (the announced one, or the
        /// datagram's source port under `implied_port`).
        stored_port: u16,
    },
    /// An `announce_peer` was refused: the token did not verify, and a
    /// KRPC 203 protocol error went back.
    TokenRefused {
        /// The sender's contact.
        contact: Contact,
        /// The swarm the announce claimed.
        info_hash: [u8; 20],
    },
    /// An unknown method was answered with KRPC error 204.
    UnknownMethod {
        /// The transaction id the error reply echoed.
        transaction_id: Vec<u8>,
    },
    /// A datagram failed to parse and was dropped as data — no reply, no
    /// crash; the loop lives on.
    Malformed {
        /// The parse failure, for the caller's log.
        reason: String,
    },
    /// A response or error datagram arrived while serving and no client
    /// transaction was waiting for it. The async engine pairs in-flight
    /// client state with the socket; this groundwork drops the stray.
    StrayReply,
    /// The read timeout elapsed with no datagram.
    Timeout,
}

/// What a `get_peers` reply hands the caller: the token for the upcoming
/// announce plus the swarm endpoints and closer contacts the node knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetPeersReply {
    /// The announce token to echo in `announce_peer`.
    pub token: Vec<u8>,
    /// Compact `IPv4` peer endpoints, most recent first.
    pub values: Vec<SocketAddr>,
    /// Compact `IPv6` peer endpoints, when the node knows dual-stack peers.
    pub values6: Vec<SocketAddr>,
    /// `IPv4` contacts closer to the swarm than the replier.
    pub nodes: Vec<Contact>,
    /// `IPv6` contacts closer to the swarm than the replier.
    pub nodes6: Vec<Contact>,
}

/// A blocking BEP 5 node: one UDP socket, a Kademlia table, a peer store,
/// and a token issuer, driven by the caller from any executor or none.
#[derive(Debug)]
pub struct DhtNode {
    socket: UdpSocket,
    self_id: NodeId,
    table: RoutingTable,
    store: PeerStore,
    tokens: TokenIssuer,
    timeout: Duration,
}

impl DhtNode {
    /// Binds an `IPv4` wildcard socket on an ephemeral port for `self_id`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] when the socket cannot bind or configure.
    pub fn bind(self_id: NodeId, timeout: Duration) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        Ok(Self {
            socket,
            self_id,
            table: RoutingTable::new(self_id),
            store: PeerStore::new(),
            tokens: TokenIssuer::new(),
            timeout,
        })
    }

    /// This node's Kademlia id.
    #[must_use]
    pub fn self_id(&self) -> NodeId {
        self.self_id
    }

    /// The bound UDP endpoint (the address peers should reach us on).
    ///
    /// # Errors
    /// Returns [`Error::Io`] when the OS cannot report the local address.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// The routing table this node has built from the queries it answered.
    #[must_use]
    pub fn table(&self) -> &RoutingTable {
        &self.table
    }

    /// Mutable table access, for callers that seed or drain contacts.
    #[must_use]
    pub fn table_mut(&mut self) -> &mut RoutingTable {
        &mut self.table
    }

    /// The peer store this node's announces have filled.
    #[must_use]
    pub fn store(&self) -> &PeerStore {
        &self.store
    }

    /// Mutable store access, for callers that seed or drain endpoints.
    #[must_use]
    pub fn store_mut(&mut self) -> &mut PeerStore {
        &mut self.store
    }

    /// The token issuer this node answers `get_peers` with.
    #[must_use]
    pub fn tokens(&self) -> &TokenIssuer {
        &self.tokens
    }

    /// Sends a `ping` and returns the replier's node id.
    ///
    /// # Errors
    /// Returns [`Error::Dht`] for timeouts and KRPC error replies, and
    /// [`Error::Io`] for socket failures.
    pub fn ping(&self, addr: SocketAddr) -> Result<NodeId> {
        let transaction_id = next_transaction_id();
        let reply = self.transact(
            &Message::Query {
                transaction_id: std::borrow::Cow::Owned(transaction_id.to_vec()),
                method: Method::Ping,
                args: QueryArgs::Ping { id: self.self_id },
            },
            addr,
        )?;
        Ok(reply.id)
    }

    /// Sends `find_node` and returns the replier's closest `IPv4` contacts
    /// for `target` (dual-stack replies ride [`DhtNode::get_peers`]'s
    /// `nodes6` shape).
    ///
    /// # Errors
    /// Same contract as [`DhtNode::ping`].
    pub fn find_node(&self, addr: SocketAddr, target: &NodeId) -> Result<Vec<Contact>> {
        let transaction_id = next_transaction_id();
        let reply = self.transact(
            &Message::Query {
                transaction_id: std::borrow::Cow::Owned(transaction_id.to_vec()),
                method: Method::FindNode,
                args: QueryArgs::FindNode {
                    id: self.self_id,
                    target: *target,
                },
            },
            addr,
        )?;
        Ok(reply.nodes)
    }

    /// Sends `get_peers` and returns the reply's token, endpoints, and
    /// closer contacts. A reply without a token is a typed error — the
    /// client cannot announce without one.
    ///
    /// # Errors
    /// Same contract as [`DhtNode::ping`].
    pub fn get_peers(&self, addr: SocketAddr, info_hash: &[u8; 20]) -> Result<GetPeersReply> {
        let transaction_id = next_transaction_id();
        let reply = self.transact(
            &Message::Query {
                transaction_id: std::borrow::Cow::Owned(transaction_id.to_vec()),
                method: Method::GetPeers,
                args: QueryArgs::GetPeers {
                    id: self.self_id,
                    info_hash: *info_hash,
                },
            },
            addr,
        )?;
        let Some(token) = &reply.token else {
            return Err(Error::Dht(
                "the get_peers reply carried no announce token".into(),
            ));
        };
        Ok(GetPeersReply {
            token: token.clone().into_owned(),
            values: reply.values,
            values6: reply.values6,
            nodes: reply.nodes,
            nodes6: reply.nodes6,
        })
    }

    /// Sends `announce_peer`, claiming `info_hash` on `port` with the
    /// `token` a `get_peers` reply issued.
    ///
    /// # Errors
    /// Returns [`Error::Dht`] for timeouts and KRPC error replies (a bad
    /// token comes back as KRPC 203), and [`Error::Io`] for socket failures.
    pub fn announce_peer(
        &self,
        addr: SocketAddr,
        info_hash: &[u8; 20],
        port: u16,
        token: &[u8],
    ) -> Result<()> {
        let transaction_id = next_transaction_id();
        self.transact(
            &Message::Query {
                transaction_id: std::borrow::Cow::Owned(transaction_id.to_vec()),
                method: Method::AnnouncePeer,
                args: QueryArgs::AnnouncePeer {
                    id: self.self_id,
                    info_hash: *info_hash,
                    port,
                    token: token.to_vec(),
                    implied_port: false,
                },
            },
            addr,
        )?;
        Ok(())
    }

    /// Answers one datagram (blocking, bounded by the node's read timeout):
    /// parse, record the sender, dispatch, and send the reply. Malformed
    /// datagrams and strays are [`ServedEvent::Malformed`] /
    /// [`ServedEvent::StrayReply`] data — the caller's loop continues.
    ///
    /// # Errors
    /// Returns [`Error::Io`] only when the socket itself fails; every
    /// protocol outcome is a [`ServedEvent`].
    pub fn serve_once(&mut self) -> Result<ServedEvent> {
        let span = tracing::debug_span!("node::serve_once");
        let _enter = span.enter();
        let mut buf = vec![0_u8; MAX_DATAGRAM_LEN];
        let (n, from) = match self.socket.recv_from(&mut buf) {
            Ok(received) => received,
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(ServedEvent::Timeout);
            }
            Err(err) => return Err(err.into()),
        };
        let message = match krpc::parse(&buf[..n]) {
            Ok(message) => message,
            Err(err) => {
                return Ok(ServedEvent::Malformed {
                    reason: err.to_string(),
                });
            }
        };
        let Message::Query {
            transaction_id,
            method,
            args,
        } = message
        else {
            return Ok(ServedEvent::StrayReply);
        };
        let (event, reply) = self.dispatch(transaction_id.as_ref(), &method, &args, from)?;
        if let Some(bytes) = reply {
            self.socket.send_to(&bytes, from)?;
        }
        Ok(event)
    }

    /// Builds the reply for a parsed query — `(event, optional reply
    /// datagram)`. Error replies (`204`, bad token) come back already
    /// encoded in the slot, everything else is encoded once on the way out.
    fn dispatch(
        &mut self,
        transaction_id: &[u8],
        method: &Method,
        args: &QueryArgs,
        from: SocketAddr,
    ) -> Result<(ServedEvent, Option<Vec<u8>>)> {
        // Unknown method (or a known one with unknown-shaped arguments —
        // the parser never produces that pair, but the dispatch stays
        // total): answer KRPC 204 / 203 with the caller's transaction id.
        let unknown = matches!(method, Method::Unknown(_)) || matches!(args, QueryArgs::Unknown);
        if unknown {
            let (code, text) = if matches!(method, Method::Unknown(_)) {
                (METHOD_UNKNOWN_ERROR, "Method Unknown")
            } else {
                (PROTOCOL_ERROR, "Malformed arguments")
            };
            let reply = krpc_error_datagram(transaction_id, code, text)?;
            let event = if matches!(method, Method::Unknown(_)) {
                ServedEvent::UnknownMethod {
                    transaction_id: transaction_id.to_vec(),
                }
            } else {
                ServedEvent::Malformed {
                    reason: "known method with unknown arguments".into(),
                }
            };
            return Ok((event, Some(reply)));
        }
        let contact =
            contact_from_args(args, from).expect("a known method always carries the sender id");
        // Every answered query records the sender, exactly what a BEP 5
        // node does with its routing table.
        let _ = self.table.insert(contact.clone());
        match args {
            QueryArgs::Ping { .. } => Ok((
                ServedEvent::Pinged { contact },
                Some(response_datagram(&Response::pong(
                    transaction_id.to_vec(),
                    self.self_id,
                ))?),
            )),
            QueryArgs::FindNode { target, .. } => {
                let nodes = self.table.closest_to(target, KBUCKET_SIZE);
                let returned = nodes.len();
                Ok((
                    ServedEvent::NeighborsServed { contact, returned },
                    Some(response_datagram(&Response::neighbors(
                        transaction_id.to_vec(),
                        self.self_id,
                        nodes,
                    ))?),
                ))
            }
            QueryArgs::GetPeers { info_hash, .. } => {
                let token = self.tokens.issue(&from).to_vec();
                let (values, values6) = self.split_stored_peers(info_hash);
                let peers_returned = values.len() + values6.len();
                let target = NodeId::from_bytes(*info_hash);
                let nodes = self.table.closest_to(&target, KBUCKET_SIZE);
                let reply = Response::get_peers(
                    transaction_id.to_vec(),
                    self.self_id,
                    token,
                    values,
                    nodes,
                );
                Ok((
                    ServedEvent::PeersServed {
                        contact,
                        info_hash: *info_hash,
                        peers_returned,
                    },
                    Some(response_datagram(&reply)?),
                ))
            }
            QueryArgs::AnnouncePeer { .. } => {
                self.dispatch_announce(transaction_id, contact, args, from)
            }
            // Handled by the `unknown` branch above.
            QueryArgs::Unknown => Ok((ServedEvent::Timeout, None)),
        }
    }

    /// Splits the stored endpoints for one swarm into the `values` (IPv4)
    /// and `values6` (IPv6) shapes the reply encodes.
    fn split_stored_peers(&self, info_hash: &[u8; 20]) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
        let (mut values, mut values6) = (Vec::new(), Vec::new());
        for peer in self.store.peers(info_hash) {
            if peer.is_ipv4() {
                values.push(*peer);
            } else {
                values6.push(*peer);
            }
        }
        (values, values6)
    }

    /// The `announce_peer` half of the dispatch: verify the token, store
    /// the claim, and answer.
    fn dispatch_announce(
        &mut self,
        transaction_id: &[u8],
        contact: Contact,
        args: &QueryArgs,
        from: SocketAddr,
    ) -> Result<(ServedEvent, Option<Vec<u8>>)> {
        let QueryArgs::AnnouncePeer {
            info_hash,
            port,
            token,
            implied_port,
            ..
        } = args
        else {
            return Ok((ServedEvent::Timeout, None));
        };
        if !self.tokens.verify(&from, token) {
            let reply = krpc_error_datagram(
                transaction_id,
                PROTOCOL_ERROR,
                "announce token did not verify",
            )?;
            return Ok((
                ServedEvent::TokenRefused {
                    contact,
                    info_hash: *info_hash,
                },
                Some(reply),
            ));
        }
        let stored_port = if *implied_port { from.port() } else { *port };
        self.store
            .announce(info_hash, SocketAddr::new(from.ip(), stored_port));
        Ok((
            ServedEvent::AnnounceAccepted {
                contact,
                info_hash: *info_hash,
                stored_port,
            },
            Some(response_datagram(&Response::announce(
                transaction_id.to_vec(),
                self.self_id,
            ))?),
        ))
    }

    /// Sends one query and waits for the matching reply: datagrams from a
    /// different source or with a different transaction id are ignored,
    /// never fatal, until the timeout runs out.
    fn transact(&self, query: &Message<'_>, addr: SocketAddr) -> Result<Response<'static>> {
        let span = tracing::debug_span!("node::transact");
        let _enter = span.enter();
        let mut datagram = Vec::with_capacity(128);
        query.encode_into(&mut datagram)?;
        self.socket.send_to(&datagram, addr)?;
        let expected = query_transaction_id(query);
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Dht(format!("no reply within {:?}", self.timeout)));
            }
            self.socket.set_read_timeout(Some(remaining))?;
            let mut buf = vec![0_u8; MAX_DATAGRAM_LEN];
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(received) => received,
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Err(Error::Dht(format!("no reply within {:?}", self.timeout)));
                }
                Err(err) => return Err(err.into()),
            };
            if !reply_from_expected_peer(from, addr) {
                continue;
            }
            match krpc::parse(&buf[..n]) {
                Ok(Message::Response(response)) if response.transaction_id.as_ref() == expected => {
                    return Ok(response);
                }
                Ok(Message::Error {
                    transaction_id,
                    code,
                    message,
                }) if transaction_id.as_ref() == expected => {
                    return Err(Error::Dht(format!(
                        "the node replied with KRPC error {code}: {message}"
                    )));
                }
                // Strays and mismatched transaction ids are noise: keep
                // waiting for the real reply until the deadline.
                Ok(_) | Err(_) => {}
            }
        }
    }
}

/// Whether a reply datagram plausibly answers the query we sent to `addr`:
/// port and address family must match. Full-address equality would reject
/// every reply from a wildcard-bound node — its [`DhtNode::local_addr`]
/// reports `0.0.0.0:port`, while the OS stamps replies with the interface
/// address (`127.0.0.1:port` on loopback) — so the port carries the match.
fn reply_from_expected_peer(from: SocketAddr, addr: SocketAddr) -> bool {
    from.port() == addr.port() && from.is_ipv4() == addr.is_ipv4()
}

/// The transaction id of an outgoing query, for reply matching.
fn query_transaction_id<'a>(query: &'a Message<'a>) -> &'a [u8] {
    match query {
        Message::Query { transaction_id, .. } => transaction_id.as_ref(),
        _ => &[],
    }
}

/// Builds a contact from a query's sender id and datagram source.
fn contact_from_args(args: &QueryArgs, from: SocketAddr) -> Option<Contact> {
    let id = match args {
        QueryArgs::Ping { id }
        | QueryArgs::FindNode { id, .. }
        | QueryArgs::GetPeers { id, .. }
        | QueryArgs::AnnouncePeer { id, .. } => *id,
        QueryArgs::Unknown => return None,
    };
    Some(Contact { id, addr: from })
}

/// Encodes a KRPC error reply datagram.
fn krpc_error_datagram(transaction_id: &[u8], code: u32, message: &str) -> Result<Vec<u8>> {
    Message::Error {
        transaction_id: std::borrow::Cow::Owned(transaction_id.to_vec()),
        code,
        message: message.to_owned(),
    }
    .encode_to_vec()
}

/// Encodes a response datagram.
fn response_datagram(response: &Response<'_>) -> Result<Vec<u8>> {
    Message::Response(response.clone()).encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(seed: u8) -> DhtNode {
        DhtNode::bind(NodeId::from_bytes([seed; 20]), Duration::from_secs(2))
            .expect("loopback bind succeeds")
    }

    #[test]
    fn ping_records_the_contact_in_the_table() {
        let server = node(0x11);
        let client = node(0x22);
        let addr = server.local_addr().expect("local addr");
        let client_addr = client.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            let mut server = server;
            let event = server.serve_once().expect("serve succeeds");
            (server, event)
        });
        let id = client.ping(addr).expect("pong");
        let (server, event) = handle.join().expect("server thread");
        assert_eq!(id, NodeId::from_bytes([0x11; 20]));
        // The datagram source carries the interface address (127.0.0.1 on
        // loopback), not the wildcard address `local_addr` reports.
        assert_eq!(
            event,
            ServedEvent::Pinged {
                contact: Contact {
                    id: NodeId::from_bytes([0x22; 20]),
                    addr: SocketAddr::from(([127, 0, 0, 1], client_addr.port())),
                }
            }
        );
        assert_eq!(server.table().len(), 1);
    }

    #[test]
    fn a_silent_peer_times_out_with_a_typed_error() {
        let client = node(0x33);
        // Bind a socket that never answers.
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let err = client
            .ping(addr)
            .expect_err("a silent peer is a typed timeout");
        assert!(err.to_string().contains("no reply"), "got {err}");
    }

    #[test]
    fn serve_once_reports_malformed_datagrams_as_data() {
        let mut server = node(0x44);
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        probe
            .send_to(b"not bencode", server.local_addr().expect("local addr"))
            .expect("sent");
        match server.serve_once().expect("serve succeeds") {
            ServedEvent::Malformed { reason } => {
                assert!(reason.contains("bencode"), "got {reason}");
            }
            other => panic!("expected a malformed event, got {other:?}"),
        }
    }

    #[test]
    fn serve_once_times_out_instead_of_hanging() {
        let mut server = node(0x55);
        assert_eq!(
            server.serve_once().expect("serve succeeds"),
            ServedEvent::Timeout
        );
    }

    #[test]
    fn stored_v6_endpoints_route_to_values6_not_values() {
        // The store records the source address family; the dispatch splits
        // values by family so a v6 endpoint is never mangled into a 6-byte
        // v4 entry.
        let mut store = PeerStore::new();
        let v6: SocketAddr = "[2001:db8::1]:6881".parse().expect("parses");
        store.announce(&[0x09; 20], v6);
        let (values, values6): (Vec<SocketAddr>, Vec<SocketAddr>) = store
            .peers(&[0x09; 20])
            .iter()
            .copied()
            .partition(std::net::SocketAddr::is_ipv4);
        assert!(values.is_empty(), "no v4 endpoint was stored");
        assert_eq!(values6, vec![v6]);
    }
}
