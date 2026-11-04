//! Benchmarks for magnet URI and metainfo parsing.

use std::borrow::Cow;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::bencode::{self, Value};
use magnetite_core::magnet;
use magnetite_core::metainfo::MetaInfo;

fn sample_torrent() -> Vec<u8> {
    let pieces = vec![0x5A_u8; 20 * 256]; // 256 pieces
    let mut files = Vec::new();
    for i in 0..64_u16 {
        files.push(Value::Dict(
            [
                (
                    Cow::Borrowed(&b"length"[..]),
                    Value::Int(i64::from(i) * 1024),
                ),
                (
                    Cow::Borrowed(&b"path"[..]),
                    Value::List(vec![
                        Value::Bytes(Cow::Owned(format!("dir{i}").into_bytes())),
                        Value::Bytes(Cow::Owned(format!("file{i}.bin").into_bytes())),
                    ]),
                ),
            ]
            .into_iter()
            .collect(),
        ));
    }
    let info = Value::Dict(
        [
            (Cow::Borrowed(&b"files"[..]), Value::List(files)),
            (
                Cow::Borrowed(&b"name"[..]),
                Value::Bytes(Cow::Borrowed(&b"sample"[..])),
            ),
            (Cow::Borrowed(&b"piece length"[..]), Value::Int(262_144)),
            (
                Cow::Borrowed(&b"pieces"[..]),
                Value::Bytes(Cow::Owned(pieces)),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let root = Value::Dict(
        [
            (
                Cow::Borrowed(&b"announce"[..]),
                Value::Bytes(Cow::Borrowed(&b"udp://tracker.example:1337/announce"[..])),
            ),
            (Cow::Borrowed(&b"info"[..]), info),
        ]
        .into_iter()
        .collect(),
    );
    bencode::encode_to_vec(&root).unwrap()
}

fn bench_parsing(c: &mut Criterion) {
    let torrent = sample_torrent();
    let magnet_uri = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=sample&tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce&tr=udp%3A%2F%2Fbackup.example%3A6969%2Fannounce&x.pe=192.168.1.1%3A6881";

    let mut group = c.benchmark_group("parse");
    group.throughput(Throughput::Bytes(torrent.len() as u64));
    group.bench_function("metainfo/64-file-torrent", |b| {
        b.iter(|| MetaInfo::parse(black_box(&torrent)).unwrap());
    });
    group.bench_function("magnet/typical-uri", |b| {
        b.iter(|| magnet::parse(black_box(magnet_uri)).unwrap());
    });
    group.finish();
}

criterion_group!(benches, bench_parsing);
criterion_main!(benches);
