//! Throughput benchmarks for the bencode codec.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::bencode::{self, Value};

/// A realistic torrent-shaped dictionary: announce metadata, ~30 file keys
/// and a few KiB of piece hashes.
fn sample_value() -> Value<'static> {
    let mut dict = BTreeMap::new();
    dict.insert(
        Cow::Borrowed(&b"announce"[..]),
        Value::Bytes(Cow::Borrowed(&b"udp://tracker.example:1337/announce"[..])),
    );
    for i in 0..32_u16 {
        let key = format!("key{i:03}");
        let value = format!("dir{i}/file{i}.bin");
        dict.insert(
            Cow::Owned(key.into_bytes()),
            Value::Bytes(Cow::Owned(value.into_bytes())),
        );
    }
    dict.insert(
        Cow::Borrowed(&b"pieces"[..]),
        Value::Bytes(Cow::Owned(vec![0u8; 2048])),
    );
    Value::Dict(dict)
}

fn bench_codec(c: &mut Criterion) {
    let value = sample_value();
    let encoded = bencode::encode_to_vec(&value).unwrap();

    let mut group = c.benchmark_group("bencode");
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("decode/sample-dict", |b| {
        b.iter(|| bencode::decode(black_box(&encoded)).unwrap());
    });
    group.bench_function("encode/sample-dict", |b| {
        b.iter(|| bencode::encode_to_vec(black_box(&value)).unwrap());
    });
    group.finish();
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);
