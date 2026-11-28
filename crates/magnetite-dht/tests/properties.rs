//! Property and end-to-end specs for the BEP 5 DHT groundwork.
//!
//! The first spec pins the plan's acceptance criterion verbatim:
//! `ARCHITECTURE.md` §9, the **M2** row:
//!
//! > **M2** | DHT (BEP 5), PEX, uTP (BEP 29) | `magnetite-dht` crate; session
//! > state machine behind `&self` + internal tasks.
//!
//! The pinned spec drives a real BEP 5 introduction over loopback UDP —
//! ping → `find_node` → `get_peers` (token) → `announce_peer` → `get_peers`
//! (values) — between two blocking [`DhtNode`]s, proving the crate named by
//! the plan exists and speaks the protocol end-to-end. The properties then
//! prove the codec round-trips and that arbitrary datagrams never panic.

use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr as _;
use std::thread;
use std::time::Duration;

use proptest::prelude::*;

use magnetite_dht::contact::{COMPACT_V4_LEN, Contact, parse_compact_nodes_v4};
use magnetite_dht::krpc::{Message, Method, QueryArgs};
use magnetite_dht::node::DhtNode;
use magnetite_dht::node_id::NodeId;
use magnetite_dht::table::KBUCKET_SIZE;

/// The DHT spec node-id fixture space: ids are fully arbitrary 20-byte
/// values, like the proptest strategies below.
fn arb_node_id() -> impl Strategy<Value = NodeId> {
    proptest::collection::vec(any::<u8>(), 20)
        .prop_map(|bytes| NodeId::from_bytes(bytes.try_into().expect("strategy fixes len at 20")))
}

/// A loopback contact with a deterministic address and a random id.
fn arb_contact() -> impl Strategy<Value = Contact> {
    (any::<u16>()).prop_map(|port| Contact {
        id: NodeId::from_bytes([0; 20]),
        addr: SocketAddr::from(([127, 0, 0, 1], port)),
    })
}

/// A random IPv4 socket address (port kept in the ephemeral range so the
/// wire encoding stays deterministic).
fn arb_v4_addr() -> impl Strategy<Value = SocketAddr> {
    (any::<u32>(), 1_u16..=u16::MAX)
        .prop_map(|(bits, port)| SocketAddr::from((Ipv4Addr::from(bits), port)))
}

/// One KRPC query over the four known BEP 5 methods, with fully random ids.
fn arb_query() -> impl Strategy<Value = Message<'static>> {
    (
        arb_transaction_id(),
        arb_node_id(),
        arb_node_id(),
        any::<u16>(),
        arb_token(),
    )
        .prop_map(|(txid, id, other, port, token)| {
            let mut method_seed = 0_u8;
            for (i, byte) in id.as_bytes().iter().enumerate() {
                method_seed =
                    method_seed.wrapping_add(byte.wrapping_add(u8::try_from(i).unwrap_or(0)));
            }
            let info_hash = *other.as_bytes();
            match method_seed % 4 {
                0 => Message::Query {
                    transaction_id: txid,
                    method: Method::Ping,
                    args: QueryArgs::Ping { id },
                },
                1 => Message::Query {
                    transaction_id: txid,
                    method: Method::FindNode,
                    args: QueryArgs::FindNode { id, target: other },
                },
                2 => Message::Query {
                    transaction_id: txid,
                    method: Method::GetPeers,
                    args: QueryArgs::GetPeers { id, info_hash },
                },
                _ => Message::Query {
                    transaction_id: txid,
                    method: Method::AnnouncePeer,
                    args: QueryArgs::AnnouncePeer {
                        id,
                        info_hash,
                        port,
                        token,
                        implied_port: false,
                    },
                },
            }
        })
}

/// A random response carrying every optional BEP 5 field in random shape.
fn arb_response() -> impl Strategy<Value = Message<'static>> {
    (
        arb_transaction_id(),
        arb_node_id(),
        arb_token(),
        proptest::collection::vec(arb_v4_addr(), 0..8),
        proptest::collection::vec(arb_contact(), 0..8),
    )
        .prop_map(|(txid, id, token, values, contacts)| {
            Message::Response(magnetite_dht::krpc::Response {
                transaction_id: txid,
                id,
                token: (!token.is_empty()).then_some(std::borrow::Cow::Owned(token)),
                values,
                values6: Vec::new(),
                nodes: contacts,
                nodes6: Vec::new(),
            })
        })
}

/// A random KRPC error reply.
fn arb_error() -> impl Strategy<Value = Message<'static>> {
    (arb_transaction_id(), 201_u32..=204, any::<u16>()).prop_map(|(txid, code, seed)| {
        Message::Error {
            transaction_id: txid,
            code,
            message: format!("generated error {seed}"),
        }
    })
}

fn arb_message() -> impl Strategy<Value = Message<'static>> {
    prop_oneof![
        3 => arb_query(),
        3 => arb_response(),
        1 => arb_error(),
    ]
}

/// BEP 5 transaction ids ride as byte strings; the client generates 2 bytes.
fn arb_transaction_id() -> impl Strategy<Value = std::borrow::Cow<'static, [u8]>> {
    proptest::collection::vec(any::<u8>(), 1..=8).prop_map(std::borrow::Cow::Owned)
}

/// Announce tokens are opaque byte strings on the wire.
fn arb_token() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=16)
}

/// Spawns `node` to answer exactly `events` datagrams on its socket and
/// hands the node back for inspection afterwards.
fn serve_n(mut node: DhtNode, events: usize) -> thread::JoinHandle<DhtNode> {
    thread::spawn(move || {
        for _ in 0..events {
            node.serve_once()
                .expect("the served datagram is answered, not dropped");
        }
        node
    })
}

/// Plan-fidelity pin: the `magnetite-dht` crate speaks BEP 5 end-to-end.
/// This spec pins `ARCHITECTURE.md` §9, the **M2** row (DHT, first-listed
/// sub-item of the milestone), by driving the full introduction flow the
/// protocol demands over loopback UDP:
///
/// ping → `find_node` → `get_peers` (token) → `announce_peer` (token echo) →
/// `get_peers` (values), with both routing tables recording the counterpart
/// contact exactly like a Kademlia node answers.
#[test]
fn loopback_pair_completes_the_bep5_introduction_and_announce_flow() {
    let timeout = Duration::from_secs(5);
    let a = DhtNode::bind(NodeId::from_bytes([0xAA; 20]), timeout).expect("node A binds");
    let b = DhtNode::bind(NodeId::from_bytes([0xBB; 20]), timeout).expect("node B binds");
    // Addresses and ids are captured before the nodes move into the serving
    // threads below.
    let a_addr = a.local_addr().expect("bound socket");
    let b_addr = b.local_addr().expect("bound socket");
    let a_id = a.self_id();
    let b_id = b.self_id();
    let info_hash = [0x42_u8; 20];

    // 1. A pings B: the pong carries B's id, and B records A as a contact.
    let server = serve_n(b, 1);
    let pong = a.ping(b_addr).expect("pong");
    assert_eq!(pong, b_id);
    let b = server.join().expect("server thread");
    assert_eq!(b.table().len(), 1, "B learned A from the ping");

    // 2. B pings A: now A's table holds B too, so A can answer find_node.
    let server = serve_n(a, 1);
    let pong = b.ping(a_addr).expect("pong");
    assert_eq!(pong, a_id);
    let a = server.join().expect("server thread");
    assert_eq!(a.table().len(), 1, "A learned B from the ping");

    // 3. B find_nodes A for B's own id: A answers with its closest contacts,
    //    which include B itself.
    let target = b_id;
    let server = serve_n(a, 1);
    let contacts = b.find_node(a_addr, &target).expect("find_node reply");
    let a = server.join().expect("server thread");
    assert!(
        contacts.iter().any(|contact| contact.id == target),
        "A's neighbors for a target near B must include B, got {contacts:?}"
    );

    // 4. B get_peers on A: A hands out a rotating announce token.
    let server = serve_n(a, 1);
    let reply = b.get_peers(a_addr, &info_hash).expect("get_peers reply");
    let a = server.join().expect("server thread");
    assert!(!reply.token.is_empty(), "A must issue a token");

    // 5. Announcing with the wrong token is refused as a KRPC 203 protocol
    //    error — data, not a crash.
    let server = serve_n(a, 1);
    let err = b
        .announce_peer(a_addr, &info_hash, 6_881, b"bogus!")
        .expect_err("a bad token must be refused");
    let a = server.join().expect("server thread");
    assert!(
        err.to_string().contains("203"),
        "the KRPC protocol error code must surface: {err}"
    );

    // 6. Announcing with the right token records B's claim with A.
    let server = serve_n(a, 1);
    b.announce_peer(a_addr, &info_hash, 6_881, &reply.token)
        .expect("announce accepted");
    let a = server.join().expect("server thread");

    // 7. A fresh get_peers returns B's announced endpoint in `values`.
    let server = serve_n(a, 1);
    let reply = b
        .get_peers(a_addr, &info_hash)
        .expect("second get_peers reply");
    let a = server.join().expect("server thread");
    assert_eq!(
        reply.values,
        vec![SocketAddr::from(([127, 0, 0, 1], 6_881))],
        "the announced endpoint must come back in values"
    );
    assert_eq!(a.store().len(), 1, "the peer store holds one swarm entry");

    // 8. An unknown method is answered with KRPC error 204 (method unknown):
    //    the raw datagram goes in, the typed error reply comes back.
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe binds");
    let server = serve_n(a, 1);
    let datagram = b"d1:q3:foo1:t2:zz1:y1:qe";
    probe
        .send_to(datagram, a_addr)
        .expect("probe datagram sent");
    let mut buf = [0_u8; 128];
    let (n, _) = probe.recv_from(&mut buf).expect("the 204 reply comes back");
    let _ = server.join().expect("server thread");
    let parsed = magnetite_dht::krpc::parse(&buf[..n]).expect("the reply parses");
    assert!(
        matches!(parsed, Message::Error { code: 204, .. }),
        "unknown methods must answer KRPC 204, got {parsed:?}"
    );
}

/// An unknown method query parses (so the node can answer 204 with the
/// caller's transaction id) and an unparseable datagram is typed data.
#[test]
fn unknown_methods_parse_and_garbage_is_typed_data() {
    let parsed = magnetite_dht::krpc::parse(b"d1:q8:impolite1:t2:ab1:y1:qe").expect("parses");
    match parsed {
        Message::Query {
            method: Method::Unknown(name),
            ..
        } => assert_eq!(&*name, "impolite"),
        other => panic!("expected an unknown-method query, got {other:?}"),
    }
    let err = match magnetite_dht::krpc::parse(b"not bencode at all") {
        Err(err) => err,
        Ok(other) => panic!("garbage must be typed data, got {other:?}"),
    };
    let rendered = err.to_string();
    assert!(
        rendered.contains("bencode") || rendered.contains("dht"),
        "garbage names its taxonomy variant: {err}"
    );
}

/// The KRPC codec round-trips every message shape it parses.
#[test]
fn krpc_messages_round_trip() {
    proptest!(|(message in arb_message())| {
        let wire = message
            .encode_to_vec()
            .expect("a valid message always encodes");
        let parsed = magnetite_dht::krpc::parse(&wire)
            .expect("the canonical encoding always parses");
        prop_assert_eq!(parsed.into_owned(), message);
    });
}

/// Arbitrary and truncated datagrams never panic: adversarial input is
/// data, not a crash (the house rule every edge of the stack pins).
#[test]
fn krpc_datagrams_never_panic() {
    proptest!(|(input in proptest::collection::vec(any::<u8>(), 0..512))| {
        let _ = magnetite_dht::krpc::parse(&input);
        if !input.is_empty() {
            let _ = magnetite_dht::krpc::parse(&input[..input.len() - 1]);
        }
    });
}

/// Compact node-info entries round-trip byte-exactly in the BEP 5 wire
/// order: 20 id bytes, then 4 address bytes, then 2 port bytes.
#[test]
fn compact_nodes_round_trip() {
    proptest!(|(ports in proptest::collection::vec(any::<u16>(), 0..16), id_byte in any::<u8>())| {
        let mut wire = Vec::new();
        let mut contacts = Vec::new();
        for (i, port) in ports.iter().enumerate() {
            let mut id = [id_byte; 20];
            id[0] = u8::try_from(i).unwrap_or(0);
            let contact = Contact {
                id: NodeId::from_bytes(id),
                addr: SocketAddr::from(([192, 0, 2, u8::try_from(i % 251).unwrap_or(0)], *port | 1)),
            };
            wire.extend_from_slice(&magnetite_dht::contact::encode_compact_node(&contact).expect("v4 encodes"));
            contacts.push(contact);
        }
        let parsed = parse_compact_nodes_v4(&wire).expect("26-byte entries parse");
        prop_assert_eq!(parsed, contacts);
    });
}

/// The routing table answers `closest_to` with the true K nearest contacts,
/// checked against a brute-force XOR sort.
#[test]
fn table_closest_are_the_true_nearest() {
    proptest!(|(seeds in proptest::collection::vec(any::<u64>(), 1..64), target in arb_node_id(), k in 1_usize..=KBUCKET_SIZE)| {
        let mut seen = std::collections::HashSet::new();
        let mut table = magnetite_dht::table::RoutingTable::new(NodeId::from_bytes([0x11; 20]));
        let mut contacts = Vec::new();
        for (i, seed) in seeds.iter().enumerate() {
            let mut bytes = [0_u8; 20];
            bytes[0..8].copy_from_slice(&seed.to_be_bytes());
            bytes[0] = u8::try_from(i).unwrap_or(0);
            let id = NodeId::from_bytes(bytes);
            if seen.insert(id) {
                let port = u16::try_from(1024 + i).unwrap_or(1024);
                let contact = Contact { id, addr: SocketAddr::from(([127, 0, 0, 1], port)) };
                let outcome = table.insert(contact.clone());
                // The brute-force reference only sees contacts the table
                // actually holds: a K-bucket at capacity refuses a candidate.
                if !matches!(outcome, magnetite_dht::table::InsertOutcome::BucketFull(_)) {
                    contacts.push(contact);
                }
            }
        }
        let got = table.closest_to(&target, k);
        let mut by_distance = contacts;
        by_distance.sort_by(|left, right| {
            left.id.distance(&target).cmp(&right.id.distance(&target))
        });
        let want: Vec<Contact> = by_distance.into_iter().take(k).collect();
        prop_assert_eq!(got, want);
    });
}

/// The pinned compact entry length: 20 id bytes + 4 IPv4 bytes + 2 port
/// bytes, checked against the constant the parser enforces.
#[test]
fn compact_v4_entry_is_twenty_six_bytes() {
    assert_eq!(COMPACT_V4_LEN, 26);
    let contact = Contact {
        id: NodeId::from_bytes([0xAB; 20]),
        addr: SocketAddr::from_str("192.0.2.7:6881").expect("parses"),
    };
    let wire = magnetite_dht::contact::encode_compact_node(&contact).expect("v4 encodes");
    assert_eq!(wire.len(), COMPACT_V4_LEN);
    assert_eq!(&wire[..4], &[0xAB; 4], "the id leads the entry");
    assert_eq!(
        &wire[20..],
        &[192, 0, 2, 7, 0x1a, 0xe1],
        "ip then port close it"
    );
}
