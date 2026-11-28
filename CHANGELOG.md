# Changelog

All notable changes to Magnetite are documented here, generated from
[Conventional Commits](https://www.conventionalcommits.org) by git-cliff.
## [unreleased]

### ⛰️  Features

- *(core)* Wire the stream session module into the crate - ([66fcb93](https://github.com/elcoosp/magnetite/commit/66fcb93482bbf5d2169ea6d99db05392d70c66e1))
- *(core)* Tokio stream session reader over the runtime-free pump - ([719e9e0](https://github.com/elcoosp/magnetite/commit/719e9e04547ef7ee32afee72224e65d5335db99d))

### 🐛 Bug Fixes

- *(core)* Make tracker transaction ids monotonic to fix bench flake - ([50d057d](https://github.com/elcoosp/magnetite/commit/50d057d10f66aa54fd227d7a073338df83850bf9))

### ⚡ Performance

- *(core)* Benchmark the stream session over a 1024-piece download - ([df3e66b](https://github.com/elcoosp/magnetite/commit/df3e66bd016b116a09c0ab965216eeaeac775189))

### 📚 Documentation

- *(core)* Consumer example wiring the stream session over tcp - ([45ded2a](https://github.com/elcoosp/magnetite/commit/45ded2addaa0932c5ce132afbb29360d2a033b52))

### ⚙️ CI / Tooling

- Gitignore the ship.toml milestone manifest - ([5a16975](https://github.com/elcoosp/magnetite/commit/5a1697532716cf94a9e184ece49d8ecd24eb091d))
- Ignore ship.toml - ([0cf726d](https://github.com/elcoosp/magnetite/commit/0cf726d84e8eecdbef7d7cc436199bfeb972331e))

### Build

- *(core)* Add the stream feature and register its bench and example - ([18403a9](https://github.com/elcoosp/magnetite/commit/18403a936a73f2fae5b0385470635c2aba5f3438))
- *(core)* Record the tokio runtime in the lockfile - ([949cd36](https://github.com/elcoosp/magnetite/commit/949cd364d1f5092d0c47a214f69832a0c17c9e0e))
- *(core)* Add the tokio runtime to the workspace dependencies - ([161730c](https://github.com/elcoosp/magnetite/commit/161730cfc76d20f482c1a490b84468c0cb744b83))
## [0.7.0](https://github.com/elcoosp/magnetite/compare/v0.5.1..v0.7.0) - 2026-11-09

### ⛰️  Features

- *(core)* Consumer example for the endgame swarm - ([f7635d5](https://github.com/elcoosp/magnetite/commit/f7635d576397c4d5ac5969a1ea60f80b9947efc1))
- *(core)* Endgame-aware planning and upload serve policy - ([94f78fe](https://github.com/elcoosp/magnetite/commit/94f78fe58c28fd207bc7b2150dae458fc730f20e))
- *(core)* Wire the endgame module and crate docs - ([c362d96](https://github.com/elcoosp/magnetite/commit/c362d963df3b190d899ec6591c6a719d4b14de65))
- *(core)* Endgame board with duplicate-request ledger - ([472798c](https://github.com/elcoosp/magnetite/commit/472798c8c3e8630de43b510d4e4cacf7b843fc08))
- *(core)* Consumer example for the requeue loop - ([a3c5db1](https://github.com/elcoosp/magnetite/commit/a3c5db1e7203b3fd8555e4a66f9e7b4b3c614445))
- *(core)* Rejected-piece requeue with typed bitfield guard - ([a557292](https://github.com/elcoosp/magnetite/commit/a557292b809c62bf2878de964c07dc335e2fe806))

### 🐛 Bug Fixes

- *(core)* Sharpen metainfo rejections and piece-length contract - ([ebffce5](https://github.com/elcoosp/magnetite/commit/ebffce556b20806bb1024bf8b8c375538a685b28))
- *(core)* Preserve surplus btih magnet parameters - ([7bf9a36](https://github.com/elcoosp/magnetite/commit/7bf9a36516eb868abdbdd316761ca2ceb2124dc9))

### ⚡ Performance

- *(core)* Endgame and upload rows on the session bench - ([82ddc7f](https://github.com/elcoosp/magnetite/commit/82ddc7f6142e9bc1482420e662abfd8e3df1af5f))
- *(core)* Requeue benchmark on the session bench - ([4f8ccb5](https://github.com/elcoosp/magnetite/commit/4f8ccb575550843975fff1a829bc74613cbb550b))

### 📚 Documentation

- *(architecture)* Record the endgame-upload milestone - ([d2868ad](https://github.com/elcoosp/magnetite/commit/d2868adc925d24c6a7f4fa87f5976511af2e02fc))
- *(architecture)* Record the requeue milestone - ([24bad16](https://github.com/elcoosp/magnetite/commit/24bad1688bc837bca9405ba909017d7578cac3d2))
- *(core)* Pin the hex/base32 BEP attribution on the infohash parser - ([518830b](https://github.com/elcoosp/magnetite/commit/518830bd4c5d30615d042e780ad70d561cce77ac))
- *(core)* Correct the bitfield-validation claim in downloader docs - ([4221d18](https://github.com/elcoosp/magnetite/commit/4221d182d35e3b2c82698445ae946f0875e20d04))
- *(core)* Reconcile peer wire rustdoc with shipped modules - ([f039465](https://github.com/elcoosp/magnetite/commit/f03946586bfe17d92ea87ecb3b9f7ec6c1b6c8a5))
- *(readme)* Refresh tagline, status and endgame rows - ([cbfa26d](https://github.com/elcoosp/magnetite/commit/cbfa26d69b39a5a3196ffaa5802443ac2636b1be))
- *(readme)* Refresh badge, status and requeue rows - ([61e588b](https://github.com/elcoosp/magnetite/commit/61e588bf6d4fe4a3668d6f0a43193b3c5524c2ef))

### 🧪 Testing

- *(core)* Endgame properties for tiling and hostility - ([11e323a](https://github.com/elcoosp/magnetite/commit/11e323a1b5817aa7a4020cd13bbfdd18153e3938))
- *(core)* Requeue and duplicate-bitfield property specs - ([5e36d5c](https://github.com/elcoosp/magnetite/commit/5e36d5c51d6bc180f21dacb06d9cb34ebe3f9743))

### ⚙️ CI / Tooling

- Version bump - ([e53d9bb](https://github.com/elcoosp/magnetite/commit/e53d9bbeb1520846a413dda1b995ec2a92834f1e))
## [0.5.1](https://github.com/elcoosp/magnetite/compare/v0.5.0..v0.5.1) - 2026-11-09

### ⛰️  Features

- *(core)* Consumer example wiring the download pump - ([9aaf52b](https://github.com/elcoosp/magnetite/commit/9aaf52b51b3bfb65209eb9c7978a2efd172a5853))
- *(core)* Per-connection session pipeline and message router - ([e1e3fec](https://github.com/elcoosp/magnetite/commit/e1e3feca96bb05b6c72c203a50dc22ac9fbd9c0a))

### 🐛 Bug Fixes

- *(core)* Surface actionable help text via miette diagnostics - ([22be3ee](https://github.com/elcoosp/magnetite/commit/22be3eec73072c21ed82320acef0cf49ef52b39e))

### 📚 Documentation

- *(architecture)* Record the session groundwork milestone - ([db72eb5](https://github.com/elcoosp/magnetite/commit/db72eb5e8356aaaaea12e31e01ca7b929a439445))
- *(readme)* Refresh badge, status and session rows - ([bbe1ff4](https://github.com/elcoosp/magnetite/commit/bbe1ff4c9dffe40fb6ae5b57feff91e0ef33418e))

### 🧪 Testing

- *(core)* Proptest specs for the session router - ([44f05e5](https://github.com/elcoosp/magnetite/commit/44f05e58756488ff39317c1815e9ac55d1723347))
## [0.5.0](https://github.com/elcoosp/magnetite/compare/v0.4.0..v0.5.0) - 2026-11-09

### ⛰️  Features

- *(core)* Piece verifier with verified piece field - ([81099da](https://github.com/elcoosp/magnetite/commit/81099da6b8eab390ab642e71a9d623f2d80d4240))

### ⚡ Performance

- *(core)* Add piece verification benchmarks - ([5bad5d5](https://github.com/elcoosp/magnetite/commit/5bad5d5afbd9836486c7c481a1b513822de418e4))

### 📚 Documentation

- *(architecture)* Record the piece verification milestone - ([8419265](https://github.com/elcoosp/magnetite/commit/8419265450547cf4cd07833ffb6460edab71197d))
- *(readme)* Refresh status and verify performance rows - ([eefb959](https://github.com/elcoosp/magnetite/commit/eefb959b7758868d7a1a68b5ce489f5d695d0b0d))

### 🧪 Testing

- *(core)* Proptest specs for the piece verifier - ([122d28d](https://github.com/elcoosp/magnetite/commit/122d28d6cfec56dfa36d85e402f524e4628c7ed0))

### Build

- *(core)* Register the verify bench target - ([7d96879](https://github.com/elcoosp/magnetite/commit/7d96879f4b3dc7d13047a9266b12ad51c40bc371))
## [0.4.0](https://github.com/elcoosp/magnetite/compare/v0.3.0..v0.4.0) - 2026-11-09

### ⛰️  Features

- *(core)* Export the downloader module - ([4ff0030](https://github.com/elcoosp/magnetite/commit/4ff003094b6faed32b269bb94601a0fedd609433))
- *(core)* Downloader groundwork: piece field, picker, assembler - ([de337d9](https://github.com/elcoosp/magnetite/commit/de337d9a9d06178ea1db3ee38784326685cacf18))
- *(core)* Add Download variant to the error taxonomy - ([43ce472](https://github.com/elcoosp/magnetite/commit/43ce4725987c3c79f96751555c56ef0e8ae79349))

### 🐛 Bug Fixes

- *(cliff)* Point changelog links at the elcoosp remote - ([c963f17](https://github.com/elcoosp/magnetite/commit/c963f17115b217c214414e7ca7a151b1d4e004c5))

### ⚡ Performance

- *(core)* Add downloader benchmarks - ([2bc971f](https://github.com/elcoosp/magnetite/commit/2bc971f647de0cbd35ab322d4d0e055f8eb8239c))

### 📚 Documentation

- *(architecture)* Record peer wire and downloader milestones - ([d314cc4](https://github.com/elcoosp/magnetite/commit/d314cc4ea5b4f486cb1033f0df4ddc341ef89ebe))
- *(readme)* Refresh status, badge and downloader performance rows - ([8304b3e](https://github.com/elcoosp/magnetite/commit/8304b3e008ab12d05b1874b6a9816870c7cf1653))

### 🧪 Testing

- *(core)* Proptest specs for the downloader state machines - ([9e7babd](https://github.com/elcoosp/magnetite/commit/9e7babd772af3284f3d41ecf770d9d52881b12b2))

### Build

- *(core)* Register the download bench target - ([dc72e21](https://github.com/elcoosp/magnetite/commit/dc72e211105689cef612906d1149a9c317b72ef0))
## [0.3.0](https://github.com/elcoosp/magnetite/compare/v0.2.0..v0.3.0) - 2026-11-09

### ⛰️  Features

- *(core)* Peer wire protocol framing (BEP 10 + BEP 3) ([#2](https://github.com/elcoosp/magnetite/issues/2)) - ([ea3e638](https://github.com/elcoosp/magnetite/commit/ea3e638f039556ad61793418cf6315aadbb8e38f))
## [0.2.0] - 2026-11-08

### ⛰️  Features

- *(bencode)* Implement span-tracking decoder - ([d20acdc](https://github.com/elcoosp/magnetite/commit/d20acdc63686050a836742ac16ea57d9cb6eeb29))
- *(cli)* Clap v4 CLI with bencode/magnet/torrent subcommands and --json - ([de04f9f](https://github.com/elcoosp/magnetite/commit/de04f9f7d85ec3b304e0cf9072dd08804f104a30))
- *(cli)* Add magnetite CLI manifest (clap, anyhow, tracing, serde_json) - ([603d859](https://github.com/elcoosp/magnetite/commit/603d8597b52c4b24c3571340ab10a2daabf4e1a9))
- *(core)* Export the udp tracker module - ([fcd6732](https://github.com/elcoosp/magnetite/commit/fcd6732d1f6b9420849cd57ce802e4ba1265d022))
- *(core)* BEP 15 UDP tracker client with compact peers - ([77e2fda](https://github.com/elcoosp/magnetite/commit/77e2fda8b09c0f04ecf4980eee20779b89dec727))
- *(core)* Add Tracker variant to the error taxonomy - ([380ef1a](https://github.com/elcoosp/magnetite/commit/380ef1ab48a71803c73e335a7a85a3c7b830deff))
- *(core)* BEP 3 .torrent parser with spec-exact infohashing - ([e96f77c](https://github.com/elcoosp/magnetite/commit/e96f77c9b06dbceddac17f4dc234ca75b5be37ba))
- *(core)* BEP 9 magnet URI parser with round-trip Display - ([c8e6544](https://github.com/elcoosp/magnetite/commit/c8e654480bda24d61362d5993de7de4fedc2c915))
- *(core)* Add crate root and module wiring - ([e0c9bf4](https://github.com/elcoosp/magnetite/commit/e0c9bf42bdca0aa1cafae9097a6883ce5f3aacf9))
- *(core)* 20-byte InfoHash with hex/base32 spellings - ([045066b](https://github.com/elcoosp/magnetite/commit/045066b76dde8cc5f05c4c50447c7d33b2964b5d))
- *(core)* Typed error taxonomy with byte-offset bencode errors - ([70cd6f4](https://github.com/elcoosp/magnetite/commit/70cd6f4984200fadd0f7132ac00c9662fc72b914))
- *(core)* Zero-copy bencode codec with depth cap and allocation guards - ([02c544b](https://github.com/elcoosp/magnetite/commit/02c544b6f48a4fca4eb0ef24d3f6287fcefbe223))
- *(core)* Add magnetite-core manifest (data-encoding, hex, percent-encoding, sha1, thiserror) - ([a3a766c](https://github.com/elcoosp/magnetite/commit/a3a766c170fd8086d01a26dbb91897a8df771ae2))
- *(metainfo)* Transition infohash derivation to raw-slice parsing - ([1a0b2fc](https://github.com/elcoosp/magnetite/commit/1a0b2fc74176f82a28ee3570982a9b17d0fa5bbf))
- Bootstrap workspace with pedantic lints and MSRV 1.85 - ([dac7b0b](https://github.com/elcoosp/magnetite/commit/dac7b0bd240f9c46b0f0e91f1c21dea8a6c73ebe))

### ⚡ Performance

- *(core)* Add tracker packet benchmarks - ([d56c79a](https://github.com/elcoosp/magnetite/commit/d56c79a60d4b3a3b797b995b2bc4a68d89dd0cf5))
- *(core)* Criterion benches for metainfo and magnet parsing - ([cdbfcfc](https://github.com/elcoosp/magnetite/commit/cdbfcfc339aa6c8f65e63320a692285507cad4e7))
- *(core)* Criterion benches for bencode encode/decode - ([c406b8f](https://github.com/elcoosp/magnetite/commit/c406b8fb5671bfae6e154c5775767b4db42f5e8e))
- Add benchmark for span-tracked bencode decoding - ([842d0ef](https://github.com/elcoosp/magnetite/commit/842d0ef7ddfac028ef7f7d7ff1ec8d26a6e069d6))

### 🚜 Refactor

- *(cli)* Drop float casting from human bytes formatter - ([b6ecac8](https://github.com/elcoosp/magnetite/commit/b6ecac84dc429c834428481204bb2121d9b2b43e))

### 📚 Documentation

- *(architecture)* Move tracker client to shipped M1 row - ([35e572c](https://github.com/elcoosp/magnetite/commit/35e572ccdbfaff362eca138d6a9982facef5bbb5))
- *(changelog)* Add git-cliff generated 0.1.0 changelog - ([cb35726](https://github.com/elcoosp/magnetite/commit/cb35726650dfd69d28709ab2ab9a61ce30dbca0f))
- *(magnet)* Clarify source spec alignment for x.pe extension field - ([60284f0](https://github.com/elcoosp/magnetite/commit/60284f0023f3653342b31f2d3c1743f01e1adfa2))
- *(readme)* Mark the tracker milestone shipped in 0.2.0 - ([de2f5d4](https://github.com/elcoosp/magnetite/commit/de2f5d47842b4c3e39021bc633a837c1887efa92))
- Adjust root library documentation contract for raw-slice hashing - ([8578619](https://github.com/elcoosp/magnetite/commit/8578619b8300ac62db439d9d8f2d65ed0e653732))
- Mirror raw-slice infohash documentation and status in readme - ([f8bd032](https://github.com/elcoosp/magnetite/commit/f8bd0326508296e010135214d0b490669c681543))
- Document raw-slice infohashing contract and benchmarks - ([c27e789](https://github.com/elcoosp/magnetite/commit/c27e789f56ed6f90178a2a91e7e3138f8136e75f))
- Add ARCHITECTURE.md (competitive analysis, M0 contracts, roadmap) - ([682f6bc](https://github.com/elcoosp/magnetite/commit/682f6bc53fc968b265e2112f013537791a288610))

### 🧪 Testing

- *(bencode)* Introduce proptest specs for span-tracked validation - ([9600aae](https://github.com/elcoosp/magnetite/commit/9600aaea1cadf92d8a661dfb28bf57bca6b94531))
- *(cli)* Assert_cmd end-to-end tests for human and --json output - ([606a60c](https://github.com/elcoosp/magnetite/commit/606a60c93844c1285e46a134a401d82e32811f4c))
- *(core)* Proptest specs for tracker packet parsing - ([475794f](https://github.com/elcoosp/magnetite/commit/475794f246cdb297041b2e90ff867155e98e05e0))
- *(core)* Proptest round-trip and never-panic properties - ([08156b3](https://github.com/elcoosp/magnetite/commit/08156b38d4cebf096e83ed6b155ce79f128dc155))

### 🎨 Styling

- *(infohash)* Satisfy pedantic clippy requirements - ([d609367](https://github.com/elcoosp/magnetite/commit/d6093670ba2ea0346a790759bd03538d18812707))

### ⚙️ CI / Tooling

- *(changelog)* Add git-cliff conventional-commit config - ([bb0bd44](https://github.com/elcoosp/magnetite/commit/bb0bd44ed356738e64191bef0b60b9b4ddde20fc))
- *(changelog)* Add workflow-dispatch job to regenerate CHANGELOG.md - ([33a2580](https://github.com/elcoosp/magnetite/commit/33a2580e827593057a914277042d5ee35eed0451))
- *(release)* Add tag-triggered cross-platform build+release - ([2be2987](https://github.com/elcoosp/magnetite/commit/2be298781f20d5bb7dc97ea5ea24477db1d07345))
- Ignore local shipping script - ([8758878](https://github.com/elcoosp/magnetite/commit/87588786f1f0d5bfbb5866a432849ae4ab6dc616))
- Enforce full pedantic clippy checks in validation gate - ([78cccd6](https://github.com/elcoosp/magnetite/commit/78cccd62eba102215b3626ad2ff63209323a39a8))
- Logo - ([0737398](https://github.com/elcoosp/magnetite/commit/0737398f93bc187feaea9edf2278cf5322584c8c))
- Readme - ([357331d](https://github.com/elcoosp/magnetite/commit/357331d3d77041866a22d3bd069d33424ab1ddf4))
- Readme - ([4fa5bcc](https://github.com/elcoosp/magnetite/commit/4fa5bcc785cd335f076a722f2252516ab37e4174))
- Pin stable Rust toolchain - ([f2130b4](https://github.com/elcoosp/magnetite/commit/f2130b49ac6b53bed987423eef826a5962177a57))
- Add MIT license - ([08e1b08](https://github.com/elcoosp/magnetite/commit/08e1b08f75e0fbd3c8abb32cd77bf524fd8223fa))
- Add Rust gitignore (target, .rs.bak, Cargo.lock.bak) - ([76f9a05](https://github.com/elcoosp/magnetite/commit/76f9a05318d81f1053c36367aebf00daf3a029dc))
- Add fmt/clippy/test/bench/doc/msrv matrix - ([55e205e](https://github.com/elcoosp/magnetite/commit/55e205eae74e717b931a17f3bcf37ca4d6ca0779))

### Build

- Bump workspace version to 0.2.0 and update lints justification - ([ac19f0c](https://github.com/elcoosp/magnetite/commit/ac19f0c368163d328232df7e48e831ca45d6d9a2))

### Bump

- Version 0.2.0 - ([0dd45e2](https://github.com/elcoosp/magnetite/commit/0dd45e2bb8887a830b19a4c388e4add495bbefba))
<!-- generated by git-cliff -->
