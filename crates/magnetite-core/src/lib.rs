//! # magnetite-core
//!
//! Magnet-first `BitTorrent` primitives: a zero-copy bencode codec, magnet URI
//! parsing, `.torrent` metainfo handling with raw-slice infohashing, and a
//! runtime-free UDP tracker client.
//!
//! ## Design contract
//!
//! - `#![forbid(unsafe_code)]` — enforced through the workspace lint table.
//! - Adversarial input is *data*, never a crash: every decoder bounds its
//!   allocations by the input length and caps nesting depth.
//! - Errors are typed ([`Error`]) and carry byte offsets where relevant.
//!
//! See `ARCHITECTURE.md` in the repository for the full design.

pub mod bencode;
pub mod error;
pub mod infohash;
pub mod magnet;
pub mod metainfo;
pub mod tracker;

pub use error::{Error, Result};
