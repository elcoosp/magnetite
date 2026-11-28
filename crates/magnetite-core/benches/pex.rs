//! Benchmarks for the PEX hot paths (BEP 10 handshake + BEP 11 `ut_pex`).
//!
//! `ut_pex` decode/encode run once per PEX message per connection (capped
//! at 50 announced contacts by BEP 11), the extended handshake runs once
//! per session setup, and the ledger delta runs once per roster change —
//! all runtime-free bookkeeping the caller batches on its own cadence.

use std::hint::black_box;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use magnetite_core::pex::{ExtHandshake, PexFlags, PexLedger, PexMessage, PexPeer};

/// Fifty IPv4 peers with flag bytes — the BEP 11 cap on announced contacts
/// per message once the initial roster is out. `seed` shifts the addresses
/// so two rosters never overlap.
fn fifty_peers(seed: u16) -> Vec<PexPeer> {
    (0..50_u16)
        .map(|n| PexPeer {
            addr: SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(
                    10,
                    u8::try_from(n / 256).unwrap_or(255),
                    u8::try_from(n).unwrap_or(255),
                    u8::try_from(seed).unwrap_or(255),
                ),
                6_881 + n,
            )),
            flags: PexFlags(if n % 3 == 0 {
                PexFlags::SUPPORTS_UTP
            } else {
                0
            }),
        })
        .collect()
}

fn bench_pex(c: &mut Criterion) {
    let mut group = c.benchmark_group("pex");

    let message = PexMessage {
        added: fifty_peers(0),
        ..PexMessage::default()
    };
    let wire = message.encode();

    group.throughput(Throughput::Bytes(wire.len() as u64));
    group.bench_function("ut_pex/decode-50-peers", |b| {
        b.iter(|| black_box(PexMessage::parse(black_box(&wire))).expect("legal shape"));
    });
    group.bench_function("ut_pex/encode-50-peers", |b| {
        b.iter(|| black_box(message.encode()));
    });

    let handshake = ExtHandshake {
        client: Some("Magnetite 0.7.0".into()),
        listen_port: Some(6_881),
        extensions: [("ut_pex".to_string(), 2_u8)].into_iter().collect(),
        max_requests: Some(250),
        ..ExtHandshake::default()
    };
    let handshake_wire = handshake.encode();
    group.throughput(Throughput::Bytes(handshake_wire.len() as u64));
    group.bench_function("ext-handshake/decode", |b| {
        b.iter(|| black_box(ExtHandshake::parse(black_box(&handshake_wire))).expect("legal shape"));
    });

    // The session's per-connection PEX bookkeeping: an initial full-roster
    // announcement followed by the swap of one 50-peer roster for another
    // (50 adds + 50 drops, right at the BEP 11 cap).
    group.throughput(Throughput::Elements(100));
    group.bench_function("ledger/initial-plus-swap-50-for-50", |b| {
        b.iter_batched(
            PexLedger::new,
            |mut ledger| {
                black_box(ledger.delta(fifty_peers(0)));
                black_box(ledger.delta(fifty_peers(1)));
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_pex);
criterion_main!(benches);
