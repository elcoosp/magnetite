//! KRPC: the bencoded request/response protocol BEP 5 rides on.
//!
//! This is the third network edge of the stack, and it keeps the house
//! rules: parsing stays runtime-free and allocation-bounded (the KRPC
//! dicts ride the hardened [`magnetite_core::bencode`] codec, so every
//! allocation is bounded by the datagram's remaining length), adversarial
//! bytes are typed errors — never panics — and there is no I/O here at all.
//! The UDP node ([`crate::node::DhtNode`]) consumes this codec directly.
//!
//! Protocol shape (BEP 5 §KRPC):
//!
//! - Every datagram is one bencoded dict with a byte-string transaction id
//!   `t` and a one-letter type `y`: `q` (query), `r` (response), `e` (error).
//! - Queries add `q` (the method name) and `a` (the arguments dict):
//!   `ping` needs `id`, `find_node` needs `id` + `target`, `get_peers`
//!   needs `id` + `info_hash`, and `announce_peer` needs `id` +
//!   `info_hash` + `token` + `port` (or `implied_port`).
//! - Responses add `r` (the return dict): `id` is required; `token`,
//!   `values`/`values6` (compact peer lists) and `nodes`/`nodes6`
//!   (compact node lists) are accepted wherever they appear, because the
//!   wire shape is method-agnostic.
//! - Errors carry `e`, a two-element list: the integer code
//!   ([`GENERIC_ERROR`]..[`METHOD_UNKNOWN_ERROR`]) and a UTF-8 message.
//!
//! Leniency boundary, in the house style: *unknown `q` method names parse*
//! (their transaction id rides along so the node can answer KRPC error
//! 204), but their arguments are deliberately not validated or preserved;
//! *unknown keys* inside `a`/`r` are ignored rather than round-tripped —
//! the groundwork's round-trip contract covers the four BEP 5 methods'
//! known fields, and extension preservation rides the same seam as
//! `peer_wire`'s `Unknown` messages in a later drop.

use std::borrow::Cow;
use std::net::SocketAddr;

use magnetite_core::bencode::{self, Value};
use magnetite_core::error::{Error, Result};

use crate::contact::{Contact, parse_compact_nodes_v4, parse_compact_nodes_v6};
use crate::node_id::NodeId;

/// Upper bound for a single KRPC datagram we are willing to parse: UDP
/// datagrams cannot exceed ~64 KiB, and the cap keeps a spoofed length
/// claim from making us look at more than that.
pub const MAX_DATAGRAM_LEN: usize = 65_536;

/// Upper bound for a transaction id we accept on the wire: BEP 5 clients
/// send 2 bytes, and anything past 8 is not a transaction id worth
/// matching replies against.
const MAX_TRANSACTION_ID_LEN: usize = 8;

/// KRPC error 201: generic.
pub const GENERIC_ERROR: u32 = 201;
/// KRPC error 202: server.
pub const SERVER_ERROR: u32 = 202;
/// KRPC error 203: protocol — malformed message, invalid arguments, or a
/// bad announce token.
pub const PROTOCOL_ERROR: u32 = 203;
/// KRPC error 204: method unknown.
pub const METHOD_UNKNOWN_ERROR: u32 = 204;

/// The `q` method name of a query, lenient about which names appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `ping` — "are you there?" with the sender's id.
    Ping,
    /// `find_node` — "who is closest to this target id?".
    FindNode,
    /// `get_peers` — "who has this info hash, and here is my announce token
    /// request?".
    GetPeers,
    /// `announce_peer` — "I have this info hash on this port, here is your
    /// token back".
    AnnouncePeer,
    /// Any other method name: parsed so the transaction id can be echoed
    /// back with KRPC error 204, arguments unvalidated.
    Unknown(Cow<'static, str>),
}

impl Method {
    /// The wire method name this variant encodes to.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Method::Ping => "ping",
            Method::FindNode => "find_node",
            Method::GetPeers => "get_peers",
            Method::AnnouncePeer => "announce_peer",
            Method::Unknown(name) => name,
        }
    }

    /// Resolves a wire method name; unknown names stay parseable.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "ping" => Method::Ping,
            "find_node" => Method::FindNode,
            "get_peers" => Method::GetPeers,
            "announce_peer" => Method::AnnouncePeer,
            other => Method::Unknown(Cow::Owned(other.to_owned())),
        }
    }
}

/// The arguments dict of a query, strict about how known methods are
/// shaped. An unknown method's arguments are not validated (or preserved),
/// so they carry no data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryArgs {
    /// `ping` arguments: the sender's node id.
    Ping {
        /// The sender's 160-bit node id.
        id: NodeId,
    },
    /// `find_node` arguments: the sender's id and the target it wants
    /// neighbors for.
    FindNode {
        /// The sender's 160-bit node id.
        id: NodeId,
        /// The id the sender wants closest contacts for.
        target: NodeId,
    },
    /// `get_peers` arguments: the sender's id and the swarm it is asking
    /// about.
    GetPeers {
        /// The sender's 160-bit node id.
        id: NodeId,
        /// The info hash of the swarm.
        info_hash: [u8; 20],
    },
    /// `announce_peer` arguments: the sender's id, the swarm, the port its
    /// `BitTorrent` peer listens on, and the token the `get_peers` reply
    /// handed out.
    AnnouncePeer {
        /// The sender's 160-bit node id.
        id: NodeId,
        /// The info hash of the swarm.
        info_hash: [u8; 20],
        /// The peer listen port the sender claims; `0` when
        /// `implied_port` is set and no port was sent.
        port: u16,
        /// The opaque token proving the sender saw a `get_peers` reply.
        token: Vec<u8>,
        /// BEP 5 `implied_port`: when set, the announced port is the UDP
        /// source port of the datagram, not the `port` field.
        implied_port: bool,
    },
    /// An unknown method's arguments: carried only so the parser can accept
    /// the datagram and answer error 204.
    Unknown,
}

/// One KRPC datagram, borrowed from the input where possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    /// A request: `y = "q"`, a method name, and an arguments dict.
    Query {
        /// The transaction id the reply must echo.
        transaction_id: Cow<'a, [u8]>,
        /// Which method the sender is asking for.
        method: Method,
        /// The validated arguments (or [`QueryArgs::Unknown`]).
        args: QueryArgs,
    },
    /// A reply: `y = "r"` and a return dict. The shape is
    /// method-agnostic on the wire, so every optional BEP 5 field is
    /// carried wherever it appears.
    Response(Response<'a>),
    /// A KRPC error reply: `y = "e"`, an integer code, and a message.
    Error {
        /// The transaction id this error answers.
        transaction_id: Cow<'a, [u8]>,
        /// One of the KRPC error codes ([`GENERIC_ERROR`]..[`METHOD_UNKNOWN_ERROR`]
        /// in practice).
        code: u32,
        /// The human-readable error text.
        message: String,
    },
}

/// The return dict of a KRPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response<'a> {
    /// The transaction id this response answers.
    pub transaction_id: Cow<'a, [u8]>,
    /// The replier's node id (required by BEP 5).
    pub id: NodeId,
    /// The announce token a `get_peers` reply hands out; opaque bytes the
    /// caller echoes back in `announce_peer`.
    pub token: Option<Cow<'a, [u8]>>,
    /// BEP 5 `values`: compact 6-byte `IPv4` peer endpoints.
    pub values: Vec<SocketAddr>,
    /// Dual-stack `values6`: compact 18-byte `IPv6` peer endpoints.
    pub values6: Vec<SocketAddr>,
    /// BEP 5 `nodes`: compact 26-byte `IPv4` node contacts closer to the
    /// queried target than the replier.
    pub nodes: Vec<Contact>,
    /// Dual-stack `nodes6`: compact 38-byte `IPv6` node contacts.
    pub nodes6: Vec<Contact>,
}

impl Response<'_> {
    /// A `ping` reply: just the replier's id.
    #[must_use]
    pub fn pong(transaction_id: Vec<u8>, id: NodeId) -> Response<'static> {
        Response {
            transaction_id: Cow::Owned(transaction_id),
            id,
            token: None,
            values: Vec::new(),
            values6: Vec::new(),
            nodes: Vec::new(),
            nodes6: Vec::new(),
        }
    }

    /// A `find_node` reply: the replier's id plus its closest contacts.
    #[must_use]
    pub fn neighbors(
        transaction_id: Vec<u8>,
        id: NodeId,
        nodes: Vec<Contact>,
    ) -> Response<'static> {
        Response {
            transaction_id: Cow::Owned(transaction_id),
            id,
            token: None,
            values: Vec::new(),
            values6: Vec::new(),
            nodes,
            nodes6: Vec::new(),
        }
    }

    /// A `get_peers` reply: token, the stored peers, and closer contacts as
    /// the fallback.
    #[must_use]
    pub fn get_peers(
        transaction_id: Vec<u8>,
        id: NodeId,
        token: Vec<u8>,
        values: Vec<SocketAddr>,
        nodes: Vec<Contact>,
    ) -> Response<'static> {
        Response {
            transaction_id: Cow::Owned(transaction_id),
            id,
            token: Some(Cow::Owned(token)),
            values,
            values6: Vec::new(),
            nodes,
            nodes6: Vec::new(),
        }
    }

    /// An `announce_peer` reply: just the replier's id.
    #[must_use]
    pub fn announce(transaction_id: Vec<u8>, id: NodeId) -> Response<'static> {
        Response::pong(transaction_id, id)
    }
}

impl Message<'_> {
    /// Detaches every borrowed byte string from the parse buffer, the same
    /// seam peer wire's `into_owned` guards across await points.
    #[must_use]
    pub fn into_owned(self) -> Message<'static> {
        match self {
            Message::Query {
                transaction_id,
                method,
                args,
            } => Message::Query {
                transaction_id: Cow::Owned(transaction_id.into_owned()),
                method: match method {
                    Method::Ping => Method::Ping,
                    Method::FindNode => Method::FindNode,
                    Method::GetPeers => Method::GetPeers,
                    Method::AnnouncePeer => Method::AnnouncePeer,
                    Method::Unknown(name) => Method::Unknown(Cow::Owned(name.into_owned())),
                },
                args: match args {
                    QueryArgs::Ping { id } => QueryArgs::Ping { id },
                    QueryArgs::FindNode { id, target } => QueryArgs::FindNode { id, target },
                    QueryArgs::GetPeers { id, info_hash } => QueryArgs::GetPeers { id, info_hash },
                    QueryArgs::AnnouncePeer {
                        id,
                        info_hash,
                        port,
                        token,
                        implied_port,
                    } => QueryArgs::AnnouncePeer {
                        id,
                        info_hash,
                        port,
                        token,
                        implied_port,
                    },
                    QueryArgs::Unknown => QueryArgs::Unknown,
                },
            },
            Message::Response(response) => Message::Response(Response {
                transaction_id: Cow::Owned(response.transaction_id.into_owned()),
                id: response.id,
                token: response.token.map(|token| Cow::Owned(token.into_owned())),
                values: response.values,
                values6: response.values6,
                nodes: response.nodes,
                nodes6: response.nodes6,
            }),
            Message::Error {
                transaction_id,
                code,
                message,
            } => Message::Error {
                transaction_id: Cow::Owned(transaction_id.into_owned()),
                code,
                message,
            },
        }
    }

    /// Appends the canonical bencoded wire form to `out`.
    ///
    /// # Errors
    /// Returns [`Error::Dht`] when the encoder's depth guard trips (never
    /// for the shapes this codec builds — the nesting is three levels).
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<()> {
        bencode::encode(&self.to_value(), out)
    }

    /// Encodes the message into a fresh canonical bencoded buffer.
    ///
    /// # Errors
    /// Same contract as [`Message::encode_into`].
    pub fn encode_to_vec(&self) -> Result<Vec<u8>> {
        bencode::encode_to_vec(&self.to_value())
    }

    /// Builds the bencode `Value` tree: a `BTreeMap`, so the encoding is
    /// canonical (bytewise sorted keys) for free.
    fn to_value(&self) -> Value<'static> {
        let mut dict = std::collections::BTreeMap::new();
        match self {
            Message::Query {
                transaction_id,
                method,
                args,
            } => {
                dict.insert(key(b"t"), bytes_value(transaction_id.as_ref()));
                dict.insert(key(b"y"), static_bytes(b"q"));
                dict.insert(key(b"q"), bytes_value(method.as_str().as_bytes()));
                let mut a = std::collections::BTreeMap::new();
                match args {
                    QueryArgs::Ping { id } => {
                        a.insert(key(b"id"), bytes_value(id.as_bytes()));
                    }
                    QueryArgs::FindNode { id, target } => {
                        a.insert(key(b"id"), bytes_value(id.as_bytes()));
                        a.insert(key(b"target"), bytes_value(target.as_bytes()));
                    }
                    QueryArgs::GetPeers { id, info_hash } => {
                        a.insert(key(b"id"), bytes_value(id.as_bytes()));
                        a.insert(key(b"info_hash"), bytes_value(info_hash.as_slice()));
                    }
                    QueryArgs::AnnouncePeer {
                        id,
                        info_hash,
                        port,
                        token,
                        implied_port,
                    } => {
                        a.insert(key(b"id"), bytes_value(id.as_bytes()));
                        a.insert(key(b"info_hash"), bytes_value(info_hash.as_slice()));
                        if *implied_port {
                            a.insert(key(b"implied_port"), Value::Int(1));
                        } else {
                            a.insert(key(b"port"), Value::Int(i64::from(*port)));
                        }
                        a.insert(key(b"token"), bytes_value(token));
                    }
                    // An unknown method carries no arguments on the wire:
                    // the reply is error 204 regardless.
                    QueryArgs::Unknown => {}
                }
                dict.insert(key(b"a"), Value::Dict(a));
            }
            Message::Response(response) => {
                dict.insert(key(b"t"), bytes_value(response.transaction_id.as_ref()));
                dict.insert(key(b"y"), static_bytes(b"r"));
                dict.insert(key(b"r"), response_value(response));
            }
            Message::Error {
                transaction_id,
                code,
                message,
            } => {
                dict.insert(key(b"t"), bytes_value(transaction_id.as_ref()));
                dict.insert(key(b"y"), static_bytes(b"e"));
                dict.insert(
                    key(b"e"),
                    Value::List(vec![
                        Value::Int(i64::from(*code)),
                        bytes_value(message.as_bytes()),
                    ]),
                );
            }
        }
        Value::Dict(dict)
    }
}

/// Builds the `r` return dict's value tree: `id` always, the optionals only
/// when non-empty (an absent list and an empty list parse identically).
fn response_value(response: &Response<'_>) -> Value<'static> {
    let mut r = std::collections::BTreeMap::new();
    r.insert(key(b"id"), bytes_value(response.id.as_bytes()));
    if let Some(token) = &response.token {
        r.insert(key(b"token"), bytes_value(token.as_ref()));
    }
    if !response.values.is_empty() {
        r.insert(
            key(b"values"),
            Value::List(
                response
                    .values
                    .iter()
                    .map(|addr| bytes_value(&compact_peer_v4_bytes(*addr)))
                    .collect(),
            ),
        );
    }
    if !response.values6.is_empty() {
        r.insert(
            key(b"values6"),
            Value::List(
                response
                    .values6
                    .iter()
                    .map(|addr| bytes_value(&compact_peer_v6_bytes(*addr)))
                    .collect(),
            ),
        );
    }
    if !response.nodes.is_empty() {
        r.insert(
            key(b"nodes"),
            bytes_value(&compact_nodes_bytes(&response.nodes)),
        );
    }
    if !response.nodes6.is_empty() {
        r.insert(
            key(b"nodes6"),
            bytes_value(&compact_nodes_bytes(&response.nodes6)),
        );
    }
    Value::Dict(r)
}

/// The transaction id generator the client side uses: a monotonic counter
/// offset by a per-process time salt, the same pattern the tracker client
/// proven against reply flake uses — strictly monotonic within a run, so
/// replies can never be matched against the wrong query.
#[must_use]
pub fn next_transaction_id() -> [u8; 2] {
    use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SALT: AtomicU32 = AtomicU32::new(u32::MAX);
    static COUNTER: AtomicU16 = AtomicU16::new(0);
    if SALT.load(Ordering::Relaxed) == u32::MAX {
        let salt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let _ = SALT.compare_exchange(u32::MAX, salt, Ordering::Relaxed, Ordering::Relaxed);
    }
    let mixed = SALT
        .load(Ordering::Relaxed)
        .wrapping_add(u32::from(COUNTER.fetch_add(1, Ordering::Relaxed)));
    let bytes = mixed.to_be_bytes();
    [bytes[2], bytes[3]]
}

/// Parses exactly one KRPC datagram; the input must be the whole datagram.
/// The returned message is fully owned: the borrowed pieces of a KRPC
/// datagram (transaction id, token) are tiny, and the node/peer lists are
/// decoded into typed structs anyway, so there is nothing worth borrowing.
///
/// # Errors
/// Returns [`Error::Dht`] when the datagram is oversized, not one bencoded
/// dict, missing or ill-shaped `t`/`y`, or a known method with malformed
/// arguments.
pub fn parse(datagram: &[u8]) -> Result<Message<'static>> {
    if datagram.len() > MAX_DATAGRAM_LEN {
        return Err(Error::Dht(format!(
            "datagram of {} bytes exceeds the {MAX_DATAGRAM_LEN}-byte cap",
            datagram.len()
        )));
    }
    let top = bencode::decode(datagram)?;
    let Value::Dict(dict) = &top else {
        return Err(Error::Dht("a KRPC datagram must be a bencoded dict".into()));
    };
    let transaction_id: Vec<u8> = match dict.get(b"t".as_slice()) {
        Some(Value::Bytes(bytes)) if bytes.len() <= MAX_TRANSACTION_ID_LEN => bytes.to_vec(),
        Some(Value::Bytes(bytes)) => {
            return Err(Error::Dht(format!(
                "transaction id is {} bytes, must be 1..={MAX_TRANSACTION_ID_LEN}",
                bytes.len()
            )));
        }
        _ => return Err(Error::Dht("KRPC datagram has no `t` transaction id".into())),
    };
    let kind = match dict.get(b"y".as_slice()) {
        Some(Value::Bytes(bytes)) => bytes.as_ref(),
        _ => return Err(Error::Dht("KRPC datagram has no `y` type".into())),
    };
    match kind {
        b"q" => {
            let method = match dict.get(b"q".as_slice()) {
                Some(Value::Bytes(bytes)) => Method::from_name(&String::from_utf8_lossy(bytes)),
                _ => return Err(Error::Dht("query has no `q` method name".into())),
            };
            let args = match dict.get(b"a".as_slice()) {
                Some(Value::Dict(arguments)) => parse_query_args(&method, arguments)?,
                _ if matches!(method, Method::Unknown(_)) => QueryArgs::Unknown,
                _ => return Err(Error::Dht("query has no `a` arguments dict".into())),
            };
            Ok(Message::Query {
                transaction_id: Cow::Owned(transaction_id),
                method,
                args,
            })
        }
        b"r" => {
            let Value::Dict(returned) = dict
                .get(b"r".as_slice())
                .ok_or_else(|| Error::Dht("response has no `r` return dict".into()))?
            else {
                return Err(Error::Dht("the `r` return value must be a dict".into()));
            };
            Ok(Message::Response(parse_return_values(
                transaction_id,
                returned,
            )?))
        }
        b"e" => {
            let Value::List(fields) = dict
                .get(b"e".as_slice())
                .ok_or_else(|| Error::Dht("error reply has no `e` list".into()))?
            else {
                return Err(Error::Dht(
                    "the `e` value must be a [code, message] list".into(),
                ));
            };
            if fields.len() != 2 {
                return Err(Error::Dht(format!(
                    "the `e` list carries {} fields, BEP 5 wants [code, message]",
                    fields.len()
                )));
            }
            let Value::Int(code) = fields[0] else {
                return Err(Error::Dht("the error code must be an integer".into()));
            };
            if code < 0 || code > i64::from(u32::MAX) {
                return Err(Error::Dht(format!("the error code {code} is out of range")));
            }
            let Value::Bytes(message) = &fields[1] else {
                return Err(Error::Dht("the error message must be a byte string".into()));
            };
            Ok(Message::Error {
                transaction_id: Cow::Owned(transaction_id),
                code: u32::try_from(code).unwrap_or(GENERIC_ERROR),
                message: String::from_utf8_lossy(message).into_owned(),
            })
        }
        other => Err(Error::Dht(format!(
            "unknown KRPC type {:?}: `y` must be `q`, `r` or `e`",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// Validates a known method's arguments dict against BEP 5; unknown methods
/// get [`QueryArgs::Unknown`] without validation.
fn parse_query_args(
    method: &Method,
    arguments: &std::collections::BTreeMap<Cow<'_, [u8]>, Value<'_>>,
) -> Result<QueryArgs> {
    match method {
        Method::Ping => Ok(QueryArgs::Ping {
            id: required_id(arguments)?,
        }),
        Method::FindNode => Ok(QueryArgs::FindNode {
            id: required_id(arguments)?,
            target: NodeId::from_bytes(required_20(arguments, b"target")?),
        }),
        Method::GetPeers => Ok(QueryArgs::GetPeers {
            id: required_id(arguments)?,
            info_hash: required_20(arguments, b"info_hash")?,
        }),
        Method::AnnouncePeer => {
            let id = required_id(arguments)?;
            let info_hash = required_20(arguments, b"info_hash")?;
            let token = match arguments.get(b"token".as_slice()) {
                Some(Value::Bytes(bytes)) => bytes.to_vec(),
                _ => {
                    return Err(Error::Dht(
                        "announce_peer needs a `token` byte string".into(),
                    ));
                }
            };
            let implied_port = match arguments.get(b"implied_port".as_slice()) {
                Some(Value::Int(1)) => true,
                Some(Value::Int(0)) | None => false,
                Some(_) => {
                    return Err(Error::Dht(
                        "announce_peer `implied_port` must be 0 or 1".into(),
                    ));
                }
            };
            let port = match arguments.get(b"port".as_slice()) {
                Some(Value::Int(port)) => u16::try_from(*port).map_err(|_| {
                    Error::Dht(format!("announce_peer port {port} is out of range"))
                })?,
                Some(_) => {
                    return Err(Error::Dht("announce_peer `port` must be an integer".into()));
                }
                None if implied_port => 0,
                None => {
                    return Err(Error::Dht(
                        "announce_peer needs a `port`, or `implied_port` = 1".into(),
                    ));
                }
            };
            Ok(QueryArgs::AnnouncePeer {
                id,
                info_hash,
                port,
                token,
                implied_port,
            })
        }
        Method::Unknown(_) => Ok(QueryArgs::Unknown),
    }
}

/// Validates the `r` return dict: `id` required, every known optional field
/// strict about shape.
fn parse_return_values(
    transaction_id: Vec<u8>,
    returned: &std::collections::BTreeMap<Cow<'_, [u8]>, Value<'_>>,
) -> Result<Response<'static>> {
    let id = required_id(returned)?;
    let token = match returned.get(b"token".as_slice()) {
        Some(Value::Bytes(bytes)) => Some(Cow::Owned(bytes.to_vec())),
        None => None,
        Some(_) => return Err(Error::Dht("the `token` must be a byte string".into())),
    };
    let values = parse_compact_peer_list(returned, b"values", 6)?;
    let values6 = parse_compact_peer_list(returned, b"values6", 18)?;
    let nodes = match returned.get(b"nodes".as_slice()) {
        Some(Value::Bytes(bytes)) => parse_compact_nodes_v4(bytes)?,
        None => Vec::new(),
        Some(_) => return Err(Error::Dht("the `nodes` must be a byte string".into())),
    };
    let nodes6 = match returned.get(b"nodes6".as_slice()) {
        Some(Value::Bytes(bytes)) => parse_compact_nodes_v6(bytes)?,
        None => Vec::new(),
        Some(_) => return Err(Error::Dht("the `nodes6` must be a byte string".into())),
    };
    Ok(Response {
        transaction_id: Cow::Owned(transaction_id),
        id,
        token,
        values,
        values6,
        nodes,
        nodes6,
    })
}

/// Parses a BEP 5 compact peer list out of the return dict: `values` is a
/// *list* of 6-byte (or 18-byte) entries, unlike `nodes` which is one
/// concatenated byte string.
fn parse_compact_peer_list(
    returned: &std::collections::BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    name: &[u8],
    entry_len: usize,
) -> Result<Vec<SocketAddr>> {
    let Some(Value::List(entries)) = returned.get(name) else {
        return Ok(Vec::new());
    };
    entries
        .iter()
        .map(|entry| match entry {
            Value::Bytes(bytes) if bytes.len() == entry_len => {
                if entry_len == 6 {
                    Ok(SocketAddr::new(
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                            bytes[0], bytes[1], bytes[2], bytes[3],
                        )),
                        u16::from_be_bytes([bytes[4], bytes[5]]),
                    ))
                } else {
                    let mut octets = [0_u8; 16];
                    octets.copy_from_slice(&bytes[0..16]);
                    Ok(SocketAddr::new(
                        std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets)),
                        u16::from_be_bytes([bytes[16], bytes[17]]),
                    ))
                }
            }
            Value::Bytes(bytes) => Err(Error::Dht(format!(
                "{} carries a {}-byte entry, compact peers are {entry_len} bytes",
                String::from_utf8_lossy(name),
                bytes.len()
            ))),
            _ => Err(Error::Dht(format!(
                "{} entries must be byte strings",
                String::from_utf8_lossy(name)
            ))),
        })
        .collect()
}

/// Reads the required 20-byte `id` argument or return field.
fn required_id(dict: &std::collections::BTreeMap<Cow<'_, [u8]>, Value<'_>>) -> Result<NodeId> {
    Ok(NodeId::from_bytes(required_20(dict, b"id")?))
}

/// Reads a required 20-byte field and rejects anything else.
fn required_20(
    dict: &std::collections::BTreeMap<Cow<'_, [u8]>, Value<'_>>,
    name: &[u8],
) -> Result<[u8; 20]> {
    match dict.get(name) {
        Some(Value::Bytes(bytes)) if bytes.len() == 20 => {
            let mut out = [0_u8; 20];
            out.copy_from_slice(bytes);
            Ok(out)
        }
        Some(Value::Bytes(bytes)) => Err(Error::Dht(format!(
            "`{}` is {} bytes, node ids and info hashes are 20",
            String::from_utf8_lossy(name),
            bytes.len()
        ))),
        _ => Err(Error::Dht(format!(
            "missing the `{}` 20-byte field",
            String::from_utf8_lossy(name)
        ))),
    }
}

/// A dict key: `Cow`'s `BTreeMap` keys are owned byte strings in the
/// encoder's value tree.
fn key(raw: &[u8]) -> Cow<'static, [u8]> {
    Cow::Owned(raw.to_vec())
}

/// Bytes payload for the value tree.
fn bytes_value(raw: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(raw.to_vec()))
}

/// Bytes payload straight from a `'static` literal: no copy.
fn static_bytes(raw: &'static [u8]) -> Value<'static> {
    Value::Bytes(Cow::Borrowed(raw))
}

/// The 6-byte compact `IPv4` spelling of a peer endpoint (BEP 23).
fn compact_peer_v4_bytes(addr: SocketAddr) -> Vec<u8> {
    let SocketAddr::V4(v4) = addr else {
        // Callers route IPv6 peers to `values6`; encoding an IPv4-typed
        // list entry from an IPv6 address is a caller bug, answered with
        // zeroes rather than a panic.
        return vec![0_u8; 6];
    };
    let mut out = Vec::with_capacity(6);
    out.extend_from_slice(&v4.ip().octets());
    out.extend_from_slice(&v4.port().to_be_bytes());
    out
}

/// The 18-byte compact `IPv6` spelling of a peer endpoint (BEP 7).
fn compact_peer_v6_bytes(addr: SocketAddr) -> Vec<u8> {
    let SocketAddr::V6(v6) = addr else {
        return vec![0_u8; 18];
    };
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(&v6.ip().octets());
    out.extend_from_slice(&v6.port().to_be_bytes());
    out
}

/// Concatenated compact node entries, the `nodes`/`nodes6` byte-string form.
fn compact_nodes_bytes(contacts: &[Contact]) -> Vec<u8> {
    let mut out = Vec::with_capacity(contacts.len() * crate::contact::COMPACT_V4_LEN);
    for contact in contacts {
        if let Ok(entry) = crate::contact::encode_compact_node(contact) {
            out.extend_from_slice(&entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping_query() -> Message<'static> {
        Message::Query {
            transaction_id: Cow::Owned(vec![0xAB, 0xCD]),
            method: Method::Ping,
            args: QueryArgs::Ping {
                id: NodeId::from_bytes([0x11; 20]),
            },
        }
    }

    #[test]
    fn canonical_ping_query_pins_the_wire_form() {
        let wire = ping_query().encode_to_vec().expect("encodes");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"d1:ad2:id20:");
        expected.extend_from_slice(&[0x11; 20]);
        expected.extend_from_slice(b"e1:q4:ping1:t2:");
        expected.extend_from_slice(&[0xAB, 0xCD]);
        expected.extend_from_slice(b"1:y1:qe");
        assert_eq!(wire, expected, "BTreeMap keys sort the dict: a, q, t, y");
    }

    #[test]
    fn parses_the_pinned_ping_query_back() {
        let wire = ping_query().encode_to_vec().expect("encodes");
        match parse(&wire).expect("parses") {
            Message::Query {
                transaction_id,
                method,
                args,
            } => {
                assert_eq!(transaction_id.as_ref(), &[0xAB, 0xCD]);
                assert_eq!(method, Method::Ping);
                assert_eq!(
                    args,
                    QueryArgs::Ping {
                        id: NodeId::from_bytes([0x11; 20])
                    }
                );
            }
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_datagram_without_a_transaction_id() {
        let err = parse(b"d1:y1:qe").expect_err("no `t`");
        assert!(err.to_string().contains("`t`"), "got {err}");
    }

    #[test]
    fn rejects_a_long_transaction_id() {
        let err = parse(b"d1:t9:1234567891:y1:qe").expect_err("9-byte `t`");
        assert!(err.to_string().contains("must be 1..=8"), "got {err}");
    }

    #[test]
    fn rejects_unknown_y_type() {
        let err = parse(b"d1:t2:ab1:y1:ze").expect_err("y = z");
        assert!(
            err.to_string().contains("must be `q`, `r` or `e`"),
            "got {err}"
        );
    }

    #[test]
    fn rejects_a_query_without_arguments() {
        let err = parse(b"d1:q4:ping1:t2:ab1:y1:qe").expect_err("no `a`");
        assert!(err.to_string().contains("`a`"), "got {err}");
    }

    #[test]
    fn rejects_a_short_node_id() {
        let err = parse(b"d1:ad2:id6:short!e1:q4:ping1:t2:ab1:y1:qe").expect_err("6-byte id");
        assert!(err.to_string().contains("20"), "got {err}");
    }

    #[test]
    fn announce_peer_without_port_or_implied_is_rejected() {
        let mut datagram = Vec::new();
        datagram.extend_from_slice(b"d1:ad2:id20:");
        datagram.extend_from_slice(&[0x11; 20]);
        datagram.extend_from_slice(b"9:info_hash20:");
        datagram.extend_from_slice(&[0x22; 20]);
        datagram.extend_from_slice(b"5:token3:abce1:q13:announce_peer1:t2:ab1:y1:qe");
        let err = parse(&datagram).expect_err("no port, no implied_port");
        assert!(err.to_string().contains("implied_port"), "got {err}");
    }

    #[test]
    fn announce_peer_with_implied_port_allows_missing_port() {
        let mut datagram = Vec::new();
        datagram.extend_from_slice(b"d1:ad2:id20:");
        datagram.extend_from_slice(&[0x11; 20]);
        datagram.extend_from_slice(b"9:info_hash20:");
        datagram.extend_from_slice(&[0x22; 20]);
        datagram
            .extend_from_slice(b"12:implied_porti1e5:token3:abce1:q13:announce_peer1:t2:ab1:y1:qe");
        match parse(&datagram).expect("implied_port fills the port") {
            Message::Query {
                args:
                    QueryArgs::AnnouncePeer {
                        port, implied_port, ..
                    },
                ..
            } => {
                assert_eq!(port, 0);
                assert!(implied_port);
            }
            other => panic!("expected an announce query, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_get_peers_response_with_values_and_nodes() {
        let mut response = Response {
            transaction_id: Cow::Owned(vec![1, 2]),
            id: NodeId::from_bytes([0x33; 20]),
            token: Some(Cow::Owned(b"tok".to_vec())),
            values: vec![SocketAddr::from(([10, 0, 0, 1], 6_881))],
            values6: Vec::new(),
            nodes: vec![Contact {
                id: NodeId::from_bytes([0x44; 20]),
                addr: SocketAddr::from(([10, 0, 0, 2], 6_882)),
            }],
            nodes6: Vec::new(),
        };
        let wire = Message::Response(response.clone())
            .encode_to_vec()
            .expect("encodes");
        match parse(&wire).expect("parses") {
            Message::Response(parsed) => assert_eq!(parsed, response),
            other => panic!("expected a response, got {other:?}"),
        }
        // Empty optionals are omitted from the wire and parse back absent.
        response.values.clear();
        response.nodes.clear();
        let wire = Message::Response(response.clone())
            .encode_to_vec()
            .expect("encodes");
        assert!(
            matches!(parse(&wire), Ok(Message::Response(_))),
            "still parses"
        );
    }

    #[test]
    fn parses_a_krpc_error_reply() {
        let wire = Message::Error {
            transaction_id: Cow::Owned(vec![9, 9]),
            code: METHOD_UNKNOWN_ERROR,
            message: "Method Unknown".into(),
        }
        .encode_to_vec()
        .expect("encodes");
        match parse(&wire).expect("parses") {
            Message::Error { code, message, .. } => {
                assert_eq!(code, 204);
                assert_eq!(message, "Method Unknown");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_error_reply_with_ragged_fields() {
        let err = parse(b"d1:eli204ee1:t2:ab1:y1:ee").expect_err("one field");
        assert!(err.to_string().contains("[code, message]"), "got {err}");
    }

    #[test]
    fn transaction_ids_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            assert!(seen.insert(next_transaction_id()));
        }
    }

    #[test]
    fn oversized_datagrams_are_refused_before_decoding() {
        let big = vec![b'd'; MAX_DATAGRAM_LEN + 1];
        let err = parse(&big).expect_err("oversized");
        assert!(err.to_string().contains("exceeds"), "got {err}");
    }
}
