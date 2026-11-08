//! Benchmarks for tracker packet construction and parsing.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::infohash::InfoHash;
use magnetite_core::tracker::{self, AnnounceParams, Event};

/// Builds a well-formed announce reply: 20 header bytes plus `peer_count`
/// compact `IPv4` peer entries.
fn announce_reply(peer_count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(20 + peer_count * 6);
    packet.extend_from_slice(&1_u32.to_be_bytes());
    packet.extend_from_slice(&42_u32.to_be_bytes());
    packet.extend_from_slice(&1_800_u32.to_be_bytes());
    packet.extend_from_slice(&3_u32.to_be_bytes());
    packet.extend_from_slice(&9_u32.to_be_bytes());
    for i in 0..peer_count {
        let octet = u8::try_from(i % 251).unwrap_or(0);
        let port = u16::try_from(6_881 + (i % 50_000)).unwrap_or(6_881);
        packet.extend_from_slice(&[10, 0, 0, octet]);
        packet.extend_from_slice(&port.to_be_bytes());
    }
    packet
}

fn sample_params() -> AnnounceParams {
    let mut params = AnnounceParams::new(
        InfoHash::from_bytes([0xAB; 20]),
        *b"-qB4550-magnetite-x!",
        6_881,
    );
    params.left = 1 << 30;
    params.event = Event::Started;
    params
}

/// Builds a well-formed connect reply for transaction id 7.
fn connect_reply() -> Vec<u8> {
    let mut packet = Vec::with_capacity(16);
    packet.extend_from_slice(&0_u32.to_be_bytes());
    packet.extend_from_slice(&7_u32.to_be_bytes());
    packet.extend_from_slice(&0x1234_5678_9abc_def0_i64.to_be_bytes());
    packet
}

fn bench_tracker(c: &mut Criterion) {
    let params = sample_params();
    let request = params.encode(0x1234_5678_9abc_def0, 42);
    let connect = connect_reply();
    let small_reply = announce_reply(50);
    let large_reply = announce_reply(502);
    let compact = announce_reply(1024)[20..].to_vec();

    let mut group = c.benchmark_group("tracker");

    group.throughput(Throughput::Bytes(connect.len() as u64));
    group.bench_function("build/connect-request", |b| {
        b.iter(|| tracker::connect_request(black_box(7)));
    });

    group.throughput(Throughput::Bytes(request.len() as u64));
    group.bench_function("build/announce-request", |b| {
        b.iter(|| params.encode(black_box(0x1234_5678_9abc_def0), black_box(42)));
    });

    group.throughput(Throughput::Bytes(connect.len() as u64));
    group.bench_function("parse/connect-response", |b| {
        b.iter(|| tracker::parse_connect_response(black_box(7), black_box(&connect)).unwrap());
    });
    group.throughput(Throughput::Bytes(small_reply.len() as u64));
    group.bench_function("parse/announce-response/50-peers", |b| {
        b.iter(|| {
            tracker::parse_announce_response(black_box(42), black_box(&small_reply)).unwrap()
        });
    });

    group.throughput(Throughput::Bytes(large_reply.len() as u64));
    group.bench_function("parse/announce-response/502-peers", |b| {
        b.iter(|| {
            tracker::parse_announce_response(black_box(42), black_box(&large_reply)).unwrap()
        });
    });

    group.throughput(Throughput::Bytes(compact.len() as u64));
    group.bench_function("parse/compact-peers/1024", |b| {
        b.iter(|| tracker::parse_compact_peers_v4(black_box(&compact)).unwrap());
    });

    group.finish();
}

criterion_group!(benches, bench_tracker);
criterion_main!(benches);
