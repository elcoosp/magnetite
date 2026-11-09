# Magnetite — Definitive Architecture (M1 groundwork, the re-request loop, and the endgame-upload groundwork)

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
4. **Raw-slice infohashing.** The infohash is SHA-1 over the `info` dict bytes exactly as they
   appear in the file, recorded by the decoder's span tracking — the same digest every mainstream
   client computes. Its test verifies against an independent SHA-1 over the raw slice, so a
   decoder bug cannot mask a hash bug.
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
│  │ +spans    │   └────┬─────┘   └─────┬──────┘  └───▲────┘  │
│  └─────┬─────┘        └───────────────┘             │      │
│        │ raw info entry span ──► sha1 ──────────────┘      │
│                                                             │
│  tracker/        peer_wire/      downloader/     verify/    │
│  ┌───────────┐   ┌──────────┐   ┌────────────┐  ┌────────┐  │
│  │ BEP 15    │   │ BEP 10   │   │ pieces,    │  │ SHA-1  │  │
│  │ udp client│   │ handshake│   │ picking,   │  │ verdict│  │
│  │ compact   │   │ message  │   │ block      │  │ per    │  │
│  │ peers     │   │ framing  │   │ assembly   │  │ piece  │  │
│  └─────┬─────┘   └────┬─────┘   └─────▲──────┘  └───▲────┘  │
│        │              │  messages     │ pieces      │       │
│  session/            └───────────────┴─────────────┘       │
│  ┌─────────────────────────────────────────────────┐       │
│  │ RequestPipeline (250 cap, chokes, cancels)      │       │
│  │ SessionPump: route messages → assembler→verifier│       │
│  └─────────────────────────────────────────────────┘       │
│                                                             │
│  endgame/                                                   │
│  ┌─────────────────────────────────────────────────┐       │
│  │ EndgameBoard: swarm duplicate-request ledger,   │       │
│  │ endgame detection; pump plan_upload policy      │       │
│  └─────────────────────────────────────────────────┘       │
│                                                             │
│  error.rs: Error { Bencode{offset,kind}, Metainfo,          │
│                    InfoHash, Magnet, Tracker, Peer,         │
│                    Download, Io }  (thiserror + miette help)│
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
│   │   ├── src/tracker.rs     # BEP 15 UDP tracker client + compact peers
│   │   ├── src/peer_wire.rs   # BEP 10 handshake + BEP 3 message framing
│   │   ├── src/downloader.rs  # piece fields, picking, block assembly
│   │   ├── src/verify.rs      # SHA-1 piece verification, verified piece field
│   │   ├── src/session.rs     # per-connection request pipeline + message router
│   │   ├── src/endgame.rs     # swarm-level endgame ledger + upload policy seam
│   │   ├── src/error.rs       # unified typed error taxonomy (+ miette help)
│   │   ├── benches/           # criterion: codec + parsing + tracker + peer_wire + download + verify + session
│   │   ├── examples/          # consumer demos: download_pump, reject_and_requeue, endgame_swarm
│   │   └── tests/properties.rs# proptest: roundtrip + never-panic
│   └── magnetite/             # the CLI binary
│       ├── src/main.rs        # clap v4 derive, one fn per command
│       └── tests/cli.rs       # end-to-end specs via assert_cmd
├── .github/workflows/         # ci.yml, release.yml, changelog.yml
├── cliff.toml                 # git-cliff conventional-changelog config
└── rust-toolchain.toml        # stable channel pin
```

## 4. Component contracts (M0 through the M1 re-request loop)

### 4.1 `bencode` — the codec

- `decode(&[u8]) -> Result<Value<'_>>`: exactly one value, no trailing bytes.
- `decode_with_entry_spans(&[u8]) -> Result<(Value<'_>, EntrySpans<'_>)>`: same grammar, plus
  the byte span of the encoded value under each top-level key (last duplicate wins, mirroring
  dictionary construction; spans are empty unless the top level is a dict).
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
- First `urn:btih:` wins as `info_hash`; other `xt` values are kept — including
  surplus `urn:btih:` parameters, which stay in `other_xt` (still shape-checked) so
  the URI round-trips losslessly.
- `tr`/`ws`/`x.pe` are percent-decoded and deduplicated; `Display`
  re-serializes canonically and round-trips (`parse(display(parse(x))) == parse(x)`).

### 4.3 `metainfo` — BEP 3

- `MetaInfo::parse` → announce fields + `TorrentInfo::{Single, Multi}` +
  `info_hash`.
- `info_hash` is SHA-1 over the raw bytes of the `info` dict (via the decoder's entry spans) —
  the same digest mainstream clients compute, for canonical and non-canonical torrents alike.
- Validation: `pieces` length multiple of 20, exactly one of `length`/`files`,
  non-negative lengths, correct dict/list shapes.
- `total_length()` / `piece_count()` derived helpers.

### 4.4 `infohash`

- Newtype over `[u8; 20]`; hex (BEP 3) and base32 (BEP 9) spellings,
  case-insensitive; `Display` = lowercase hex.

### 4.5 `downloader` — the piece downloader heart (groundwork)

- Runtime-free bookkeeping between the peer wire codec and the future
  socket session: no I/O, no new dependencies, `Error::Download` for
  every adversarial input.
- `PieceField`: a piece availability bitset in the BEP 3 wire bit order.
  Parsing is strict about shape — bits beyond the piece count are a typed
  error, matching how `Message::Bitfield` payloads are validated — and
  queries are total (out-of-range reads answer, never panic).
- `Availability`: per-piece peer counts feeding rarest-first selection;
  `add_peer`/`remove_peer`/`update` (for `Have` messages) with saturating
  decrements.
- `PiecePicker`: sequential (lowest missing index) and rarest-first
  (scarcest first, ties to the lowest index; degrades to sequential
  without an `Availability`).
- `BlockAssembler` + `BlockPlan`: tiles every piece into `BLOCK_LEN` =
  16 KiB blocks, files incoming blocks out of order and across pieces,
  and emits each piece's bytes exactly once per assembly for the
  caller's SHA-1 verification. Guards: `MAX_PIECES` (2²⁴) bounds bitfield allocation,
  `MAX_BUFFERED_PIECES` (256) bounds partial-piece memory, and hostile
  blocks (misaligned, wrong length, duplicate, beyond the piece) are
  typed errors. Request bookkeeping (pipeline depth, cancels) stays on
  the per-connection side; the assembler is swarm-wide truth. A burned
  (assembled) piece is reopened by `reset_piece` — the inverse the
  re-request loop needs (§4.7); partial pieces are never resettable.

### 4.6 `verify` — piece verification (groundwork)

- `PieceVerifier::new(pieces, piece_count)` borrows the metainfo's
  concatenated 20-byte SHA-1 digests (`PIECE_HASH_LEN`) — zero copies, the
  same byte strings stay borrowed. Construction rejects non-multiple-of-20
  slices, digest counts that disagree with the piece count, and counts
  beyond `MAX_PIECES` (the shared guard re-arms through `PieceField::new`).
- `verify_piece(index, piece)` is the verdict: SHA-1 over the assembled
  bytes; `Ok(true)` marks the piece verified, `Ok(false)` is a mismatch —
  *data, not a crash* — and marks nothing, leaving the piece retryable for
  the session's re-request loop. Out-of-range indexes and re-verifying an
  already-verified piece are typed `Error::Download` values.
- `expected_hash(index)`, `verified_count()`, `is_complete()`, and
  `verified_field()` expose the digest table and the verified `PieceField`
  the future session acts on (announce, ban, persist).
- Piece shape — lengths and block tiling — stays the `BlockAssembler`'s
  contract: a wrong-length piece simply hashes to the wrong digest and
  reports `Ok(false)`. The session pump is `assembler → verifier`, still
  runtime-free; `tokio` enters only at the I/O edge.

### 4.7 `session` — per-connection request pipeline and message router (groundwork)

- Runtime-free per-peer bookkeeping, the side the downloader explicitly
  deferred: no I/O, no clocks, no executor. `tokio` still enters only at
  the I/O edge; the future stream session is a thin pump over
  `SessionPump::handle_message` / `SessionPump::next_request`.
- `RequestPipeline` tracks this connection's in-flight block requests
  under `MAX_PIPELINE_DEPTH` = 250 (BEP 3 practice): duplicates are typed
  errors, every incoming `Piece` releases its request, `Choke` evicts all
  in-flight requests (returned, ordered, for re-routing to other peers),
  and `Unchoke` re-arms requesting. `cancel_request` returns the wire
  `Cancel` with the layout's block length.
- `SessionPump` routes every [`Message`]: `Bitfield`/`Have` update the
  peer's claims and the availability counts, `Piece` payloads flow
  `assembler → verifier` and surface as `PieceVerified` (bytes for
  storage) or `PieceRejected` (data: the piece is recorded in
  `rejected()` and never picked again — the assembler has burned it, so
  re-assembly belongs to the session remainder's re-request loop).
  `Cancel`/`KeepAlive`/`Interested`/`NotInterested`/`Port`/`Request`/
  unknown ids route to control events or `Ignored` — unknown ids stay
  preserved at the codec layer for extension round-trips.
- Outbound planning consumes the `PieceStrategy` over the peer's
  have-list (sequential ascending, rarest-first scarcest-first with ties
  to the lowest index), skipping blocks already in flight or already
  served, so repeated `next_request` calls tile a piece in `BLOCK_LEN`
  steps without re-requesting.
- The new seams carry `tracing` spans (`session::handle_message`,
  `session::next_request`) — no-ops until a subscriber is installed — and
  the error taxonomy every seam surfaces is a `miette::Diagnostic` with
  stable codes and actionable help text.
- A duplicate `Bitfield` on one connection is a typed `Error::Download`
  (BEP 3 sends the bitfield once, right after the handshake): only an
  *accepted* bitfield arms the guard, so a rejected shape violation
  never blocks the well-formed one, and swarm counts are never
  double-counted by a duplicate.
- The re-request loop closes the rejection verdict: `retry_rejected`
  drains the rejection list, reopens each piece through the assembler's
  `reset_piece` (`PieceField::clear_have` unmarks it), and returns the
  requeued indexes — one corrupt delivery costs a retry, not the
  download. A clean delivery then verifies as usual; a second call
  requeues nothing.
- A runnable consumer demo chains the whole stack: `examples/download_pump.rs`
  builds a torrent in memory, round-trips its magnet URI, and drives the
  pump against a fake peer (`cargo run -p magnetite-core --example
  download_pump`); `examples/reject_and_requeue.rs` serves one corrupted
  block and walks the requeue loop to completion; and
  `examples/endgame_swarm.rs` races two pumps over one shared endgame
  board, then turns the finished leech into a seeder.

### 4.8 `endgame` — the swarm-level duplicate-request ledger (groundwork)

- The pieces the per-connection pump deliberately never owned: request
  bookkeeping is per-connection, but *waste* is swarm-wide. Two pumps that
  request the same block double the bandwidth cost; in the endgame — every
  missing block already chased somewhere, the download stalled on the
  slowest peer — that duplication is exactly what mainstream clients want.
- `EndgameBoard` is the shared ledger that makes the policy honest: one
  board per torrent, shared by every pump of the swarm. It mirrors the
  assembler's layout (16 KiB tiling, one short final piece — property
  specs cross-check the two tilings), tracks delivered blocks and
  per-block request counts, and answers the two questions the stream
  session will ask: is the endgame on (`in_endgame`: missing > 0 and
  every missing block chased), and which block is worth duplicating
  (`pick_endgame`: fewest chasers first, ties to the lowest piece and
  offset, mirroring the downloader's rarest-first ranking).
- Fresh requests go through the ledger too: `next_request_endgame` walks
  the pump's strategy-ordered candidates but only plans blocks that are
  still missing *and unchased*, so pre-endgame duplication cannot happen
  by accident. Duplicates are capped at `MAX_REQUESTS_PER_BLOCK` = 3 —
  past a handful the bandwidth is pure waste — and a request this
  connection already holds is never duplicated on that connection.
- The caller feeds outcomes back: `mark_block` (delivered — leaves the
  missing set, drops outstanding demand), `mark_piece` (verified —
  guarded by the total `is_done` query so a racing duplicate delivery
  stays data), `release_request` (choke eviction, cancel), and
  `reset_piece` (the swarm-level half of the pump's re-request loop).
  Every guard is a typed `Error::Download`; every query is total.
- The upload side is the pump's `plan_upload` policy seam, runtime-free:
  an inbound request is judged against the layout (out-of-range pieces,
  non-block begins, and wrong slice lengths are typed errors) and the
  verified pieces. Magnetite serves only verified bytes — an unverified
  piece is a `ServeDecision::PieceNotVerified` refusal, data, not an
  error — and a `Serve` decision names the exact `BlockPlan` to slice out
  of the caller's storage (the pump holds no bytes; storage stays M3).
  `tracing` spans ride the new seams (`session::next_request_endgame`,
  `session::plan_upload`), and the runnable demo is
  `examples/endgame_swarm.rs`.
- With this groundwork, every bookkeeping decision the M1 remainder named
  — chokes (the pump), endgame (the board), upload side (the policy) — is
  runtime-free; the stream reader is purely the `tokio` I/O edge.

## 5. Error model

One taxonomy, zero stringly-typed boundaries:

```
Error
├── Bencode { offset: usize, kind: BencodeErrorKind }   // 6 fine-grained kinds
├── Metainfo(String)                                    // semantic violations
├── InfoHash(String)
├── Magnet(String)
├── Tracker(String)                                     // BEP 15 wire + timeouts
├── Peer(String)                                        // handshake + framing
├── Download(String)                                    // piece bookkeeping + verification + session
└── Io(io::Error)                                       // #[from]
```

Every variant is also a `miette::Diagnostic`: stable codes
(`magnetite::bencode` … `magnetite::io`) and actionable per-variant help
text, so embedders and the CLI can render reports without parsing prose
(`Io` carries only the code — the wrapped `io::Error` already says what
failed). The CLI wraps these in `anyhow` contexts (`reading {path}`) so users see the
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

## 7. Performance findings (M1 groundwork baselines, this machine, criterion)

| Benchmark | Throughput | Latency |
| --- | --- | --- |
| `bencode/decode/sample-dict` (~2.9 KiB) | **1.58 GiB/s** | 1.74 µs |
| `bencode/decode-with-spans/sample-dict` | **0.94 GiB/s** | 2.92 µs |
| `bencode/encode/sample-dict` | **1.52 GiB/s** | 1.81 µs |
| `parse/metainfo/64-file-torrent` (~8.1 KiB) | **333 MiB/s** | 23.1 µs |
| `parse/magnet/typical-uri` | **11.3 GiB/s** | 0.66 µs |

The M0 baselines (1.51 GiB/s / 1.41 GiB/s / 236 MiB/s / 11.3 GiB/s) were recorded with the
canonical re-encode on the infohash path. Span tracking removed that pass: metainfo parsing is
~30% faster and, more importantly, the hash now matches other clients byte-for-byte. The
remaining metainfo cost is deliberate allocation (`pieces` copy and per-file path strings); an
`Arc<[u8]>` piece-sharing pass is still an open M1 optimization target. Tracker, peer wire,
downloader, verification, session, endgame, and upload-policy baselines are tracked in the
README's performance table; the endgame-upload rows were measured on the sandbox runner for
this drop (a fully chased 1,024-piece tail: scarcest pick ~144 µs, duplicate request ~292 µs,
ledger record/release ~24 ns, upload serve policy ~5 ns).

## 8. Release engineering

- **CI** (`.github/workflows/ci.yml`): `fmt`, `clippy -D warnings -W clippy::pedantic --all-features` ×
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
| **M1 (groundwork — shipped in 0.2.0)** | Span-tracked decode, raw-slice infohashing | `decode_with_entry_spans` records top-level entry spans in the existing single pass; `MetaInfo::parse` hashes the raw `info` slice. Runtime-free parsing layer unchanged. |
| **M1 (tracker — shipped in 0.2.0)** | UDP tracker client (BEP 15), compact peer lists (BEP 23/7) | Blocking `std::net::UdpSocket` only: wire layouts parsed as fixed-size slices out of the datagram, transaction-id checks, one shared `Error::Tracker` variant — no async runtime, no new dependencies. |
| **M1 (peer wire framing — shipped in 0.3.0)** | BEP 10 handshake, BEP 3 message framing | Pure codec over the caller's buffer: 68-byte handshake with `parse_prefix` for stream chunks, length-prefixed messages bounded by `MAX_MESSAGE_LEN`, unknown ids preserved for extension round-trips, `into_owned` as the seam the future stream reader crosses at await points. |
| **M1 (downloader groundwork — shipped in 0.4.0)** | Piece fields, rarest-first picking, block assembly | Runtime-free bookkeeping (§4.5): strict BEP 3 bit-order piece fields, saturating availability counts, sequential/rarest-first `PiecePicker`, and a bounded `BlockAssembler` that emits each piece once for SHA-1 verification. `tokio` still enters only at the I/O edge; the session becomes a thin pump. |
| **M1 (verification — shipped in 0.5.0)** | SHA-1 verdicts over assembled pieces, verified piece field | Runtime-free closure of the assembler's contract (§4.6): `PieceVerifier` borrows the metainfo `pieces` digests, reports mismatches as `Ok(false)` data that stays retryable, and accumulates verified state in a `PieceField`; no new dependencies, `sha1` already in the tree. |
| **M1 (session groundwork — shipped in 0.5.1)** | Per-connection request pipeline and message router | Runtime-free per-peer bookkeeping (§4.7): a 250-request `RequestPipeline` with choke eviction and cancel bookkeeping, and a `SessionPump` that routes every wire message through `assembler → verifier` and swarm state; `tracing` spans on the seams, `miette` help text on the taxonomy, and a compiled consumer example. `tokio` still enters only at the I/O edge. |
| **M1 (re-request loop — shipped in 0.6.0)** | Rejected pieces are requeued and retried clean | `retry_rejected` drains the rejection list, reopens each burned piece through the assembler's `reset_piece`, and returns the requeued indexes — one corrupt delivery costs a retry, not the download (§4.7). |
| **M1 (endgame + upload groundwork — shipped in 0.6.0)** | Swarm-level duplicate-request ledger, upload serve policy | The last runtime-free seams the remainder named (§4.8): a shared `EndgameBoard` detects the endgame and caps duplicates at 3 per block, `next_request_endgame` hands out fresh blocks without cross-connection duplicates, and `plan_upload` refuses unverified pieces as data while naming the exact block to serve from caller storage. The stream reader is now purely the `tokio` I/O edge. |
| **M1 (remainder)** | Peer wire session stream reader (the `tokio` I/O edge) | `tokio` enters *only* at the I/O edge; codec, bookkeeping, routing, verification, endgame, and the upload policy all stay runtime-free, so the reader is a thin pump over `handle_message` / `next_request(_endgame)` / `plan_upload`. |
| **M2** | DHT (BEP 5), PEX, uTP (BEP 29) | `magnetite-dht` crate; session state machine behind `&self` + internal tasks. |
| **M3** | Storage abstraction (multi-file, sparse, resume) | `Storage` trait so embedding apps can back it with anything; verify pipeline streams pieces without re-reading. |
| **M4** | Library API hardening, fuzzing in CI (`cargo-fuzz`), semver checks | Fuzz targets derived from `tests/properties.rs` strategies. |

## 10. Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| Stack exhaustion on hostile bencode | `MAX_DEPTH` on both decoder and encoder, unit + property tested |
| Memory blow-up via forged length headers | Length validated against remaining input before allocation |
| Infohash drift vs other clients | Hash verified against an independent raw-slice SHA-1 in tests; the raw-slice contract is documented in §4.3 |
| Pedantic-lint debt freezing development | Curated, documented allows at the workspace level; contributors never need inline `#[allow]` for style. The full-pedantic release gate (`-D warnings -W clippy::pedantic` on the command line, which overrides the table allows) is kept green by satisfying every pedantic lint in code — `#[must_use]` getters, backticked doc terms, elided lifetimes. |
| CI minutes exploding with benches | `cargo bench -- --test` smoke in CI; real measurement local/nightly only |
