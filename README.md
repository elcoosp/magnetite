# Magnetite

A **magnet-first BitTorrent toolkit** in Rust: an embeddable library
(`magnetite-core`) and a CLI (`magnetite`) built for the best performance and
developer experience in its class.

[![CI](https://github.com/magnetite-rs/magnetite/actions/workflows/ci.yml/badge.svg)](https://github.com/magnetite-rs/magnetite/actions/workflows/ci.yml)
[![Release](https://github.com/magnetite-rs/magnetite/actions/workflows/release.yml/badge.svg)](https://github.com/magnetite-rs/magnetite/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-dea584.svg)](Cargo.toml)

## Why

Existing torrent tooling forces a bad trade: battle-tested but ancient
codebases, modern engines with painful APIs, or JS tools with no performance
story. Magnetite steals what's stealable from rqbit, anacrolix/torrent and
webtorrent — then rebuilds it zero-copy, runtime-free at the parsing layer,
and clippy-pedantic clean. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Install

```bash
cargo install magnetite
```

Or grab a prebuilt binary from the [releases](https://github.com/magnetite-rs/magnetite/releases) page.

## CLI usage

```bash
# Inspect a .torrent
$ magnetite torrent info ubuntu-24.04.iso.torrent
name        : ubuntu-24.04.iso
infohash    : 2c61616da47bb31f4db4b1f5e3b8d3f2b1e5e8a9
size        : 5.8 GiB (6223159296 bytes)
pieces      : 29793 × 256.0 KiB
files       : 1
trackers    : 3

# Break a magnet URI into typed fields (or add --json)
$ magnetite magnet parse "magnet:?xt=urn:btih:c12fe1c0...&dn=file.iso&tr=udp%3A%2F%2Ftracker%3A1337"

# Round-trip bencode through JSON
$ echo "d3:fooi42ee" | magnetite bencode decode
{ "foo": 42 }
$ echo '{"list": [1, "two"]}' | magnetite bencode encode
d4:listli1e3:twoee
```

## Library usage

```rust
use magnetite_core::bencode;
use magnetite_core::magnet;
use magnetite_core::metainfo::MetaInfo;

// Zero-copy bencode
let value = bencode::decode(b"d3:fooi42ee")?;
assert_eq!(value.get(b"foo").and_then(|v| v.as_int()), Some(42));

// Magnet URIs (hex or base32 infohashes)
let uri = magnet::parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a")?;

// .torrent files with spec-exact infohashing
let meta = MetaInfo::parse(&raw_bytes)?;
println!("{} has infohash {}", meta.total_length(), meta.info_hash);
# Ok::<(), magnetite_core::Error>(())
```

## Performance

Criterion baselines from M0 (see `crates/magnetite-core/benches/`):

| Benchmark | Throughput |
| --- | --- |
| bencode decode (torrent-shaped dict) | 1.5 GiB/s |
| bencode encode | 1.4 GiB/s |
| metainfo parse (64-file torrent) | 236 MiB/s |
| magnet URI parse | 11.3 GiB/s |

## Development

```bash
cargo test --workspace          # 38+ specs (unit, property, e2e)
cargo clippy --workspace --all-targets -- -D warnings   # pedantic, enforced
cargo bench --workspace -- --test                       # bench smoke
cargo bench -p magnetite-core --bench codec             # real numbers
```

- **TDD house style**: specs are committed red, implementations green.
- **Conventional Commits** + [git-cliff](https://git-cliff.org) changelogs;
  releases are tag-triggered (`v*`) with cross-platform binaries.
- MSRV **1.85** (edition 2024), checked in CI.
- `unsafe` is forbidden workspace-wide; adversarial input is bounded by
  construction (depth caps, length-validated allocations).

## License

MIT — see [LICENSE](LICENSE).
