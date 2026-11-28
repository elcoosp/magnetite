//! # magnetite-dht
//!
//! BEP 5 distributed-hash-table groundwork for the magnetite stack: the
//! KRPC bencoded message codec, compact node-info parsing, a Kademlia
//! routing table with K-buckets, rotating announce tokens, a bounded peer
//! store, and a blocking [`std::net`] UDP node that can both ask and answer.
//!
//! ## Plan fidelity
//!
//! `ARCHITECTURE.md` §9, the **M2** row, pins the first sub-item of this
//! milestone:
//!
//! > **M2** | DHT (BEP 5), PEX, uTP (BEP 29) | `magnetite-dht` crate; session
//! > state machine behind `&self` + internal tasks.
//!
//! This drop lands the **DHT (BEP 5)** groundwork in its own
//! `magnetite-dht` crate, exactly the way every M1 protocol edge shipped:
//! parsing stays runtime-free and allocation-bounded (the KRPC dicts ride
//! the hardened `magnetite_core::bencode` codec), adversarial datagrams are
//! typed errors — never panics — and the only I/O is blocking
//! [`std::net::UdpSocket`], so embedders can drive it from any executor or
//! none at all. The async half of the plan row — the session state machine
//! behind `&self` with internal tasks (iterative lookups, bootstrap, table
//! refresh) — stays future work, as do PEX and uTP.
//!
//! ## House rules
//!
//! - `#![forbid(unsafe_code)]` — enforced through the workspace lint table.
//! - Adversarial input is *data*: a truncated or malformed KRPC datagram is
//!   a typed [`magnetite_core::Error::Dht`], never a panic; every
//!   allocation is bounded by the datagram's remaining length.
//! - One error taxonomy: DHT failures surface as
//!   [`magnetite_core::Error::Dht`] with a stable diagnostic code and
//!   actionable help text, like every other edge of the stack.
//! - Known query methods are strictly shaped; an *unknown* `q` method
//!   parses (carrying its transaction id) so the node can answer KRPC
//!   error 204 — its arguments are deliberately not validated or preserved.
//!
//! ## What works end-to-end today
//!
//! Two nodes on a loopback can ping, exchange routing-table neighbors via
//! `find_node`, hand out announce tokens via `get_peers`, and record
//! `announce_peer` claims a `get_peers` later returns — see
//! `examples/dht_bootstrap.rs` (`cargo run -p magnetite-dht --example
//! dht_bootstrap`). The unit specs in this crate pin the wire shapes against
//! BEP 5, and property tests prove arbitrary datagrams never panic.

pub mod contact;
pub mod krpc;
pub mod node;
pub mod node_id;
pub mod store;
pub mod table;

pub use contact::{COMPACT_V4_LEN, COMPACT_V6_LEN, Contact};
pub use krpc::{Message, Method, QueryArgs, Response};
pub use node::{DhtNode, GetPeersReply, ServedEvent};
pub use node_id::NodeId;
pub use store::{MAX_PEERS_PER_INFOHASH, PeerStore, TOKEN_LEN, TokenIssuer};
pub use table::{InsertOutcome, KBUCKET_SIZE, RoutingTable};
