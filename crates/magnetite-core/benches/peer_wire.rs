//! Benchmarks for peer wire handshake and message framing.

use std::borrow::Cow;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::infohash::InfoHash;
use magnetite_core::peer_wire::{self, Handshake, Message};

fn sample_handshake() -> Handshake {
    Handshake::new(InfoHash::from_bytes([0xAB; 20]), *b"-qB4550-magnetite-x!")
}

fn bench_peer_wire(c: &mut Criterion) {
    let handshake = sample_handshake();
    let encoded_handshake = handshake.encode();

    let request = Message::Request {
        index: 9,
        begin: 65_536,
        length: 16_384,
    };
    let encoded_request = request.encode();

    let piece = Message::Piece {
        index: 1,
        begin: 0,
        block: Cow::Owned(vec![0x5A_u8; 16_384]),
    };
    let encoded_piece = piece.encode();

    let bitfield = Message::Bitfield(Cow::Owned(vec![0xF0_u8; 1_024]));
    let encoded_bitfield = bitfield.encode();

    let mut group = c.benchmark_group("peer-wire");

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_handshake.len()).unwrap_or(0),
    )));
    group.bench_function("build/handshake", |b| {
        b.iter(|| black_box(&handshake).encode());
    });

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_handshake.len()).unwrap_or(0),
    )));
    group.bench_function("parse/handshake", |b| {
        b.iter(|| Handshake::parse(black_box(&encoded_handshake)).unwrap());
    });

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_request.len()).unwrap_or(0),
    )));
    group.bench_function("build/request", |b| {
        b.iter(|| black_box(&request).encode());
    });

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_request.len()).unwrap_or(0),
    )));
    group.bench_function("parse/request", |b| {
        b.iter(|| peer_wire::parse_message(black_box(&encoded_request)).unwrap());
    });

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_piece.len()).unwrap_or(0),
    )));
    group.bench_function("build/piece-16k", |b| {
        b.iter(|| black_box(&piece).encode());
    });

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_piece.len()).unwrap_or(0),
    )));
    group.bench_function("parse/piece-16k", |b| {
        b.iter(|| peer_wire::parse_message(black_box(&encoded_piece)).unwrap());
    });

    group.throughput(Throughput::Bytes(u64::from(
        u32::try_from(encoded_bitfield.len()).unwrap_or(0),
    )));
    group.bench_function("parse/bitfield-8k-pieces", |b| {
        b.iter(|| peer_wire::parse_message(black_box(&encoded_bitfield)).unwrap());
    });

    group.finish();
}

criterion_group!(benches, bench_peer_wire);
criterion_main!(benches);
