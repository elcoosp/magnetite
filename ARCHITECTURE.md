# Magnetite — Definitive Architecture (M0)

> Status: **implemented as described** — this document is generated *from* the
> repository, not the other way around. Every claim below is enforced by code,
> tests, or CI.

Magnetite is a **magnet-first BitTorrent toolkit**: an embeddable Rust library
(`magnetite-core`) plus a batteries-included CLI (`magnetite`). It exists
because the existing open-source options force a bad trade: battle-tested
clients with 90s-era codebases (libtorrent, Transmission), or modern engines
that trade ergonomics for throughput (rqbit, anacrolix/torrent), or
JavaScript tools that give up performance entirely (webtorrent).

---

## 1. Competitive analysis — what was stolen, what was rejected

The following repositories were cloned and studied during the redesign
(`rqbit`, `anacrolix/torrent`, `webtorrent`):

| Project | Stolen (inspiration) | Rejected |
| --- | --- | --- |
| [rqbit](https://github.com/ikatson/rqbit) | Small, single-purpose workspace crates (`bencode`, `buffers`, `sha1w`, `librqbit`) — each concern compiles, tests, and benchmarks in isolation. | Deep `tokio` coupling in the parsing layer, which makes the codec unusable without a runtime. |
| [anacrolix/torrent](https://github.com/anacrolix/torrent) (Go) | BEP-oriented module naming (`bep14`, `bep27`, `bep40`) so protocol extensions map 1:1 to files. | A single giant `client.go`; we split by concern instead. |
| [webtorrent](https://github.com/webtorrent/webtorrent) | Radical DX focus: the common case must be one call / one command. | The JS runtime; and mutable shared state behind event emitters. |
| libtorrent / Transmission (studied via docs) | Aggressive hot-path allocation discipline; the idea that parsing must never allocate per byte. | C++ memory-model complexity and 20-year API accretion. |

**Net design decisions:**

1. **Parsing is runtime-free.** `magnetite-core` M0 depends on no async
   runtime; the codec works on plain `&[u8]`. (`Stealable` perf + DX win:
   parsing a torrent should never need an executor.)
2. **Byte strings stay borrowed.** The bencode decoder returns
   `Cow<'a, [u8]>` — zero copies for well-formed input, owned values only
   when callers construct them.
3. **Canonicalization for free.** Dicts parse into `BTreeMap`, which makes
   re-encoding canonical (byte-wise sorted keys) and gives O(log n) lookups —
   exactly what infohashing and future DHT work need.

## 2. Principles

1. **Adversarial input is data, not a crash.** Every decoder bounds
   allocations by remaining input length and caps recursion depth. A forged
   `999999999:` length header errors out; it never allocates.
2. **`unsafe` is forbidden.** `workspace.lints.rust.unsafe_code = "forbid"`.
3. **Pedantic clippy from day one.** `[workspace.lints.clippy] pedantic`
   + CI `-D warnings`. The three blanket allows in the workspace manifest are
   documented trade-offs, not sloppiness.
4. **Spec-exactness where it matters.** The infohash is computed over the
   canonical bencode encoding of the `info` dict, and its test verifies
   against SHA-1 over the *raw* slice — encoder bugs cannot mask hash bugs.
5. **DX is a feature.** Typed errors with byte offsets, docs on every public
   item (`missing_docs = warn`), `--json` on every CLI command, MSRV enforced
   in CI.

## 3. System overview

```
┌─────────────────────────────────────────────────────────────┐
│                       magnetite (CLI)                       │
│   bencode decode|encode · magnet parse · torrent info       │
│   --json everywhere · anyhow contexts · tracing logs        │
└──────────────┬──────────────────────────────────────────────┘
               │ typed API
┌──────────────▼──────────────────────────────────────────────┐
│                     magnetite-core                          │
│                                                             │
│  bencode/         magnet/         metainfo/      infohash/  │
│  ┌───────────┐   ┌──────────┐   ┌────────────┐  ┌────────┐  │
│  │ decode→   │   │ parse    │   │ parse      │  │ hex    │  │
│  │  Value<Cow>│  │ →MagnetUri│  │ →MetaInfo  │  │ base32 │  │
│  │ encode←   │   │ Display  │   │ info_hash  │  │ [u8;20]│  │
│  └─────┬─────┘   └────┬─────┘   └─────┬──────┘  └───▲────┘  │
│        └───────────────┴───────────────┘             │      │
│                 canonical encode ──► sha1 ───────────┘      │
│                                                             │
│  error.rs: Error { Bencode{offset,kind}, Metainfo,          │
│                    InfoHash, Magnet, Io }  (thiserror)      │
└─────────────────────────────────────────────────────────────┘
```

### Workspace layout

```
magnetite/
├── crates/
│   ├── magnetite-core/        # the library (no runtime deps)
│   │   ├── src/bencode.rs     # zero-copy codec + MAX_DEPTH guard
│   │   ├── src/infohash.rs    # 20-byte digest, hex/base32 spellings
│   │   ├── src/magnet.rs      # BEP 9 magnet URI parser/serializer
│   │   ├── src/metainfo.rs    # BEP 3 .torrent parser + infohashing
│   │   ├── src/error.rs       # unified typed error taxonomy
│   │   ├── benches/           # criterion: codec + parsing
│   │   └── tests/properties.rs# proptest: roundtrip + never-panic
│   └── magnetite/             # the CLI binary
│       ├── src/main.rs        # clap v4 derive, one fn per command
│       └── tests/cli.rs       # end-to-end specs via assert_cmd
├── .github/workflows/         # ci.yml, release.yml, changelog.yml
├── cliff.toml                 # git-cliff conventional-changelog config
└── rust-toolchain.toml        # stable channel pin
```

## 4. Component contracts (M0)

### 4.1 `bencode` — the codec

- `decode(&[u8]) -> Result<Value<'_>>`: exactly one value, no trailing bytes.
- `Value<'a>`: `Int(i64) | Bytes(Cow<'a,[u8]>) | List | Dict(BTreeMap)`.
- Guards:
  - `MAX_DEPTH = 512` on decode **and** encode (the encoder must not blow the
    stack on deeply-nested owned values either).
  - Byte-string length is validated against remaining input **before**
    allocation.
  - Canonical integer/length forms enforced (`i03e`, `i-0e`, `03:x` rejected).
- Errors carry the **byte offset** (`Error::Bencode { offset, kind }`).

### 4.2 `magnet` — BEP 9

- Lenient about *which* parameters appear (unknown keys preserved for
  round-tripping), strict about *how* they are shaped.
- First `urn:btih:` wins as `info_hash`; other `xt` values are kept.
- `tr`/`ws`/`x.pe` are percent-decoded and deduplicated; `Display`
  re-serializes canonically and round-trips (`parse(display(parse(x))) == parse(x)`).

### 4.3 `metainfo` — BEP 3

- `MetaInfo::parse` → announce fields + `TorrentInfo::{Single, Multi}` +
  `info_hash`.
- Validation: `pieces` length multiple of 20, exactly one of `length`/`files`,
  non-negative lengths, correct dict/list shapes.
- `total_length()` / `piece_count()` derived helpers.

### 4.4 `infohash`

- Newtype over `[u8; 20]`; hex (BEP 3) and base32 (BEP 9) spellings,
  case-insensitive; `Display` = lowercase hex.

## 5. Error model

One taxonomy, zero stringly-typed boundaries:

```
Error
├── Bencode { offset: usize, kind: BencodeErrorKind }   // 6 fine-grained kinds
├── Metainfo(String)                                    // semantic violations
├── InfoHash(String)
├── Magnet(String)
└── Io(io::Error)                                       // #[from]
```

The CLI wraps these in `anyhow` contexts (`reading {path}`) so users see the
full causal chain.

## 6. Testing strategy

| Layer | Tool | What it proves |
| --- | --- | --- |
| Unit specs (TDD: red → green per module) | `#[cfg(test)]` | Grammar, canonicalization, field extraction, error offsets |
| Property tests | `proptest` | `encode(decode(x)) == x`; arbitrary and truncated input **never panics** |
| End-to-end | `assert_cmd` | The real binary's human + `--json` output, exit codes |
| Benchmarks | `criterion` | Throughput baselines; `cargo bench -- --test` smoke in CI |
| Docs | `missing_docs` + `RUSTDOCFLAGS=-D warnings` | Every public item documented, docs compile |

Every module was built test-first: the failing specs were committed, then the
implementation landed in the next commit with the suite green. The git
history *is* the TDD log.

## 7. Performance findings (M0 baselines, this machine, criterion)

| Benchmark | Throughput | Latency |
| --- | --- | --- |
| `bencode/decode/sample-dict` (~2.7 KiB) | **1.51 GiB/s** | 1.82 µs |
| `bencode/encode/sample-dict` | **1.41 GiB/s** | 1.95 µs |
| `parse/metainfo/64-file-torrent` (~7.6 KiB) | **236 MiB/s** | 32.6 µs |
| `parse/magnet/typical-uri` | **11.3 GiB/s** | 0.67 µs |

The metainfo path pays for `pieces` copying (`to_vec`) and canonical
re-encoding for the infohash — both deliberate; correctness first, and both
are optimization targets for M1 (span-tracking decoder to hash the raw
slice, and `Arc<[u8]>` piece sharing).

## 8. Release engineering

- **CI** (`.github/workflows/ci.yml`): `fmt`, `clippy -D warnings` ×
  {linux, macos, windows}, `test --all-features` × 3 OS, `cargo bench -- --test`
  smoke, `cargo doc` with `-D warnings`, MSRV 1.85 check.
  Actions pinned to current majors: `checkout@v7`, `upload-artifact@v7`,
  `download-artifact@v8`, `Swatinem/rust-cache@v2`, `dtolnay/rust-toolchain@stable`.
- **Releases** (`release.yml`): tag `v*` → cross-platform builds (linux x64,
  macos arm64 + x64 cross-compiled, windows x64) → `git-cliff` release notes →
  GitHub release with archives.
- **Changelog**: `cliff.toml` groups Conventional Commits
  (`feat/fix/perf/refactor/docs/test/ci`); CHANGELOG.md is generated, never
  hand-edited; `changelog.yml` regenerates on demand.
- **Dependabot**: weekly `cargo` + `github-actions` bumps.

## 9. Roadmap after M0

| Milestone | Scope | Key architecture decision |
| --- | --- | --- |
| **M1** | Tracker client (BEP 3/7/23), peer wire protocol (BEP 3/10), piece downloader | `tokio` enters *only* at the I/O edge; codec stays runtime-free. Span-tracking decoder replaces canonical re-encode for infohashing. |
| **M2** | DHT (BEP 5), PEX, uTP (BEP 29) | `magnetite-dht` crate; session state machine behind `&self` + internal tasks. |
| **M3** | Storage abstraction (multi-file, sparse, resume) | `Storage` trait so embedding apps can back it with anything; verify pipeline streams pieces without re-reading. |
| **M4** | Library API hardening, fuzzing in CI (`cargo-fuzz`), semver checks | Fuzz targets derived from `tests/properties.rs` strategies. |

## 10. Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| Stack exhaustion on hostile bencode | `MAX_DEPTH` on both decoder and encoder, unit + property tested |
| Memory blow-up via forged length headers | Length validated against remaining input before allocation |
| Infohash drift vs other clients | Hash verified against raw-slice SHA-1 in tests; canonical encoding documented |
| Pedantic-lint debt freezing development | Curated, documented allows at the workspace level; contributors never need inline `#[allow]` for style |
| CI minutes exploding with benches | `cargo bench -- --test` smoke in CI; real measurement local/nightly only |
