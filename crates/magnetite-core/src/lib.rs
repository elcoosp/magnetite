//! # magnetite-core
//!
//! Magnet-first `BitTorrent` primitives: a zero-copy bencode codec, magnet URI
//! parsing, `.torrent` metainfo handling with raw-slice infohashing, a
//! runtime-free UDP tracker client, the runtime-free peer wire protocol
//! codec (BEP 10 handshake, BEP 3 message framing), the runtime-free piece
//! downloader bookkeeping (piece fields, rarest-first picking, block
//! assembly), the runtime-free piece verification that judges assembled
//! pieces against the metainfo's per-piece SHA-1 hashes, the runtime-free
//! per-connection session pump that turns wire messages into download state
//! (request pipeline, message router) and requeues rejected pieces for a
//! clean retry, and the runtime-free swarm-level endgame ledger plus upload
//! serve policy that let many pumps share one torrent honestly — fresh
//! blocks handed out without duplicates, a stalled tail duplicated on
//! purpose, and inbound requests answered only with verified bytes.
//!
//! ## Design contract
//!
//! - `#![forbid(unsafe_code)]` — enforced through the workspace lint table.
//! - Adversarial input is *data*, never a crash: every decoder bounds its
//!   allocations by the input length and caps nesting depth.
//! - Errors are typed ([`Error`]) and carry byte offsets where relevant,
//!   plus stable diagnostic codes and actionable help text via
//!   `miette::Diagnostic`.
//!
//! See `ARCHITECTURE.md` in the repository for the full design.

pub mod bencode;
pub mod downloader;
pub mod endgame;
pub mod error;
pub mod infohash;
pub mod magnet;
pub mod metainfo;
pub mod peer_wire;
pub mod session;
pub mod tracker;
pub mod verify;

pub use error::{Error, Result};
