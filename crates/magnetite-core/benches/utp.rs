//! Benchmarks for the uTP hot paths: the fixed-header codec, the
//! extension walk, and the send/receive bookkeeping the future socket
//! edge drives per datagram.

use std::borrow::Cow;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::utp::{
    Connection, DEFAULT_MSS, EXT_SELECTIVE_ACK, Extension, Header, Packet, PacketType,
};

/// A canonical `ST_DATA` header on connection 0xBEEF, seq 1000 acking 999,
/// advertising 1 MiB — the shape the receive loop sees most.
fn data_header() -> Header {
    Header {
        packet_type: PacketType::Data,
        connection_id: 0xBEEF,
        timestamp_us: 1_700_000_000,
        timestamp_diff_us: 1_234,
        window_size: 1 << 20,
        seq_nr: 1000,
        ack_nr: 999,
    }
}

/// A connected initiator↔listener pair for the stream benches.
fn pair() -> (Connection, Connection) {
    let mut initiator = Connection::connect(0x4242);
    let syn = initiator.syn_packet(0);
    let listener = Connection::accept_syn(&syn.header);
    let answer = listener.state_packet(1);
    initiator.receive(&answer.encode(), 2).expect("handshake");
    (initiator, listener)
}

fn bench_utp(c: &mut Criterion) {
    let mut group = c.benchmark_group("utp");

    let header = data_header();
    let wire = header.encode();
    group.throughput(Throughput::Bytes(wire.len() as u64));
    group.bench_function("header/parse", |b| {
        b.iter(|| black_box(Header::parse(black_box(&wire))).expect("legal header"));
    });
    group.bench_function("header/encode", |b| {
        b.iter(|| black_box(header.encode()));
    });

    // The BEP 34 selective-ack walk: a 32-byte mask acking 256 scattered
    // packets — the link a healthy receiver answers every other round.
    let mask = vec![0b1010_0101_u8; 32];
    let packet = Packet {
        header,
        extensions: vec![Extension {
            kind: EXT_SELECTIVE_ACK,
            contents: Cow::Owned(mask),
        }],
        payload: Cow::Owned(vec![7_u8; 64]),
    };
    let selective_wire = packet.encode();
    group.throughput(Throughput::Bytes(selective_wire.len() as u64));
    group.bench_function("packet/parse-with-selective-ack", |b| {
        b.iter(|| black_box(Packet::parse(black_box(&selective_wire))).expect("legal packet"));
    });

    // One MSS-sized outbound packet: chunking, window pacing math, and
    // the in-flight registration the ack path retires.
    let payload = vec![7_u8; DEFAULT_MSS];
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function("send/one-mss-packet", |b| {
        b.iter_batched(
            pair,
            |(mut sender, _receiver)| {
                let packets = sender.send(black_box(&payload), 0);
                assert_eq!(packets.len(), 1);
                black_box(packets);
            },
            BatchSize::SmallInput,
        );
    });

    // An in-order 64 KiB stream: 64 × 1 KiB datagrams through the receive
    // path — bookkeeping, backlog checks, and the ack the receiver
    // answers each round with.
    let chunk = vec![7_u8; 1024];
    group.throughput(Throughput::Bytes(64 * 1024));
    group.bench_function("receive/in-order-64kib", |b| {
        b.iter_batched(
            pair,
            |(mut sender, mut receiver)| {
                for _ in 0..64 {
                    let packet = sender.send(black_box(&chunk), 0).remove(0);
                    receiver.receive(&packet.encode(), 0).expect("in order");
                    let ack = receiver.state_packet(0);
                    sender.receive(&ack.encode(), 0).expect("ack");
                }
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_utp);
criterion_main!(benches);
