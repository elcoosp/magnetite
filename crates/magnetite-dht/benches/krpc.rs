//! Benchmarks for the KRPC codec and the Kademlia routing table.
//!
//! The wire rows pin the codec's cost at realistic datagram sizes — a
//! `get_peers` query and a 50-peer reply are the hot shapes a real node
//! serves — and the table row pins `closest_to` over a fully populated
//! table (128 contacts spread over buckets), the shape every `find_node`
//! reply and the future iterative lookup pay.

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use magnetite_dht::contact::Contact;
use magnetite_dht::krpc::{Message, Method, QueryArgs, Response};
use magnetite_dht::node_id::NodeId;
use magnetite_dht::table::RoutingTable;

/// A node id with deterministic bytes derived from `seed`.
fn node_id(seed: u8) -> NodeId {
    let mut bytes = [0_u8; 20];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = seed.wrapping_add(u8::try_from(index).unwrap_or(0));
    }
    NodeId::from_bytes(bytes)
}

/// A loopback contact at `port`.
fn contact(seed: u8, port: u16) -> Contact {
    Contact {
        id: node_id(seed),
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
    }
}

/// The `get_peers` query the client half sends: 20-byte id + 20-byte info
/// hash over a small bencode dict.
fn get_peers_query() -> Message<'static> {
    Message::Query {
        transaction_id: std::borrow::Cow::Owned(vec![0x13, 0x37]),
        method: Method::GetPeers,
        args: QueryArgs::GetPeers {
            id: node_id(0x01),
            info_hash: [0x42; 20],
        },
    }
}

/// A `get_peers` reply with `peer_count` stored endpoints and 8 contacts,
/// the hot reply shape a populated node serves.
fn get_peers_response(peer_count: usize) -> Message<'static> {
    let values = (0..peer_count)
        .map(|i| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                u16::try_from(1024 + i).unwrap_or(1024),
            )
        })
        .collect();
    let nodes = (0..8)
        .map(|i| {
            contact(
                u8::try_from(i + 1).unwrap_or(1),
                u16::try_from(2000 + i).unwrap_or(2000),
            )
        })
        .collect();
    Message::Response(Response {
        transaction_id: std::borrow::Cow::Owned(vec![0x13, 0x37]),
        id: node_id(0x02),
        token: Some(std::borrow::Cow::Owned(vec![0xEE; 8])),
        values,
        values6: Vec::new(),
        nodes,
        nodes6: Vec::new(),
    })
}

/// A table with `contacts` well-spread contacts, filling buckets from the
/// middle of the space outward.
fn populated_table(contacts: usize) -> RoutingTable {
    let mut table = RoutingTable::new(node_id(0x80));
    let mut inserted = 0_usize;
    for seed in 0_u8..=255 {
        if inserted >= contacts {
            break;
        }
        let mut bytes = [0_u8; 20];
        bytes[0] = seed;
        bytes[1] = seed.wrapping_mul(7);
        let port = u16::try_from(1024 + u32::from(seed)).unwrap_or(1024);
        let outcome = table.insert(Contact {
            id: NodeId::from_bytes(bytes),
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        });
        if matches!(
            outcome,
            magnetite_dht::table::InsertOutcome::Inserted
                | magnetite_dht::table::InsertOutcome::Refreshed(_)
        ) {
            inserted += 1;
        }
    }
    table
}

fn bench_krpc_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("krpc");
    let query = get_peers_query();
    let wire = query.encode_to_vec().expect("encodes");
    group.throughput(Throughput::Bytes(u64::try_from(wire.len()).unwrap_or(0)));
    group.bench_function("encode/get-peers-query", |b| {
        b.iter(|| black_box(&query).encode_to_vec().expect("encodes"));
    });
    let response = get_peers_response(50);
    let response_wire = response.encode_to_vec().expect("encodes");
    group.throughput(Throughput::Bytes(
        u64::try_from(response_wire.len()).unwrap_or(0),
    ));
    group.bench_function("encode/get-peers-response-50-peers", |b| {
        b.iter(|| black_box(&response).encode_to_vec().expect("encodes"));
    });
    group.finish();
}

fn bench_krpc_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("krpc");
    let query_wire = get_peers_query().encode_to_vec().expect("encodes");
    group.throughput(Throughput::Bytes(
        u64::try_from(query_wire.len()).unwrap_or(0),
    ));
    group.bench_function("parse/get-peers-query", |b| {
        b.iter(|| {
            let parsed = magnetite_dht::krpc::parse(black_box(&query_wire)).expect("parses");
            black_box(parsed.into_owned())
        });
    });
    let response_wire = get_peers_response(50).encode_to_vec().expect("encodes");
    group.throughput(Throughput::Bytes(
        u64::try_from(response_wire.len()).unwrap_or(0),
    ));
    group.bench_function("parse/get-peers-response-50-peers", |b| {
        b.iter(|| {
            let parsed = magnetite_dht::krpc::parse(black_box(&response_wire)).expect("parses");
            black_box(parsed.into_owned())
        });
    });
    group.finish();
}

fn bench_table_closest(c: &mut Criterion) {
    let mut group = c.benchmark_group("table");
    let table = populated_table(128);
    let target = node_id(0x37);
    group.bench_function("closest-pick/128-contacts-k8", |b| {
        b.iter(|| black_box(&table).closest_to(black_box(&target), 8));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_krpc_encode,
    bench_krpc_parse,
    bench_table_closest,
);
criterion_main!(benches);
