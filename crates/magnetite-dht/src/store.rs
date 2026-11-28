//! The announce-token issuer and the bounded peer store a BEP 5 node keeps.
//!
//! BEP 5 makes `get_peers` the gateway to `announce_peer`: the answering
//! node hands out an opaque *token* with the reply, and the announce is
//! only honored if it echoes a token the node issued — that is what stops
//! arbitrary hosts from filling swarms with junk endpoints. The issuer
//! derives tokens from a per-node secret and a rotating time slot (SHA-1
//! over secret ‖ slot ‖ requester IP), and accepts the current *or* the
//! previous slot, the grace window BEP 5 recommends.
//!
//! The peer store is the swarm-side ledger: per info hash, a bounded
//! list of announced `IPv4` endpoints, most recent first. IPv6 announces
//! ride the same seam in a later dual-stack drop.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use sha1::{Digest, Sha1};

use crate::contact::Contact;

/// Wire length of an issued token: 8 bytes of the digest, small enough for
/// the wire, large enough that forging needs the secret.
pub const TOKEN_LEN: usize = 8;

/// Rotating announce-token issuer: the BEP 5 defense against bogus
/// announces.
#[derive(Debug, Clone)]
pub struct TokenIssuer {
    secret: [u8; 16],
    /// Seconds one token slot stays valid; the previous slot is still
    /// accepted for the grace window BEP 5 recommends.
    slot_seconds: u64,
}

impl TokenIssuer {
    /// An issuer with a time-and-pid seeded secret: distinct per process,
    /// deterministic within one process's lifetime.
    #[must_use]
    pub fn new() -> Self {
        // Seconds carry the bulk of the entropy and the sub-second nanos
        // the low bits; both are already `u64`-wide, so no cast is needed.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                (since.as_secs() << 20) ^ u64::from(since.subsec_nanos())
            });
        let pid = u32::to_be_bytes(std::process::id());
        let mut secret = [0_u8; 16];
        secret[0..8].copy_from_slice(&now.to_be_bytes());
        secret[8..12].copy_from_slice(&pid);
        secret[12..16].copy_from_slice(&pid);
        Self::with_secret(secret)
    }

    /// An issuer with a fixed secret — for tests and embedders that want
    /// reproducible tokens across restarts.
    #[must_use]
    pub fn with_secret(secret: [u8; 16]) -> Self {
        Self {
            secret,
            slot_seconds: 300,
        }
    }

    /// Issues the token for `requester` right now.
    #[must_use]
    pub fn issue(&self, requester: &SocketAddr) -> [u8; TOKEN_LEN] {
        self.issue_for_slot(requester, self.current_slot())
    }

    /// Whether `token` is one this issuer handed to `requester` in the
    /// current or the previous slot.
    #[must_use]
    pub fn verify(&self, requester: &SocketAddr, token: &[u8]) -> bool {
        let slot = self.current_slot();
        token == self.issue_for_slot(requester, slot).as_slice()
            || token
                == self
                    .issue_for_slot(requester, slot.saturating_sub(1))
                    .as_slice()
    }

    /// The time slot the current tokens belong to.
    fn current_slot(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() / self.slot_seconds)
    }

    /// SHA-1(secret ‖ slot ‖ requester IP), truncated to [`TOKEN_LEN`].
    fn issue_for_slot(&self, requester: &SocketAddr, slot: u64) -> [u8; TOKEN_LEN] {
        let mut hasher = Sha1::new();
        hasher.update(self.secret);
        hasher.update(slot.to_be_bytes());
        hasher.update(match requester {
            SocketAddr::V4(v4) => v4.ip().octets().to_vec(),
            SocketAddr::V6(v6) => v6.ip().octets().to_vec(),
        });
        let digest = hasher.finalize();
        let mut token = [0_u8; TOKEN_LEN];
        token.copy_from_slice(&digest[0..TOKEN_LEN]);
        token
    }
}

impl Default for TokenIssuer {
    /// `TokenIssuer::new()`.
    fn default() -> Self {
        Self::new()
    }
}

/// Upper bound for one swarm's stored peers: past this the bandwidth spent
/// handing out `values` stops being worth it, and the cap keeps one hot
/// swarm from ballooning the store.
pub const MAX_PEERS_PER_INFOHASH: usize = 64;

/// The announced-peer ledger: per info hash, the most recent endpoints
/// first, capped at [`MAX_PEERS_PER_INFOHASH`].
#[derive(Debug, Clone, Default)]
pub struct PeerStore {
    entries: BTreeMap<[u8; 20], Vec<SocketAddr>>,
}

impl PeerStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an announced endpoint for `info_hash`, most recent first.
    /// Re-announcing the same endpoint moves it back to the front.
    pub fn announce(&mut self, info_hash: &[u8; 20], addr: SocketAddr) {
        let entry = self.entries.entry(*info_hash).or_default();
        entry.retain(|existing| *existing != addr);
        entry.insert(0, addr);
        entry.truncate(MAX_PEERS_PER_INFOHASH);
    }

    /// The endpoints recorded for `info_hash`, most recent first.
    #[must_use]
    pub fn peers(&self, info_hash: &[u8; 20]) -> &[SocketAddr] {
        self.entries.get(info_hash).map_or(&[], Vec::as_slice)
    }

    /// How many swarms have at least one announced endpoint.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no swarm has any announced endpoint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops everything a contact announced — the cleanup the routing
    /// table's eviction path calls when a node leaves the swarm.
    pub fn forget(&mut self, contact: &Contact) {
        for peers in self.entries.values_mut() {
            peers.retain(|peer| *peer != contact.addr);
        }
        self.entries.retain(|_, peers| !peers.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};

    use super::*;

    fn localhost(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn issued_tokens_verify_and_others_do_not() {
        let issuer = TokenIssuer::with_secret([7; 16]);
        let addr = localhost(6_881);
        let token = issuer.issue(&addr);
        assert_eq!(token.len(), TOKEN_LEN);
        // The token binds to the requester's IP (BEP 5), so the same host on
        // a different port still verifies, but a different host does not.
        assert!(issuer.verify(&localhost(6_882), &token));
        let other_host = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 9)),
            6_881,
        );
        assert!(!issuer.verify(&other_host, &token));
        // Garbage tokens never verify.
        assert!(!issuer.verify(&addr, b"bogus!12"));
        assert!(!issuer.verify(&addr, &[]));
    }

    #[test]
    fn secret_rotation_changes_tokens() {
        let first = TokenIssuer::with_secret([1; 16]);
        let second = TokenIssuer::with_secret([2; 16]);
        let addr = localhost(6_881);
        assert_ne!(first.issue(&addr), second.issue(&addr));
    }

    #[test]
    fn announce_records_most_recent_first_and_dedups() {
        let mut store = PeerStore::new();
        assert!(store.is_empty());
        store.announce(&[0x01; 20], localhost(6_881));
        store.announce(&[0x01; 20], localhost(6_882));
        store.announce(&[0x01; 20], localhost(6_881));
        assert_eq!(store.peers(&[0x01; 20])[0], localhost(6_881));
        assert_eq!(store.peers(&[0x01; 20]).len(), 2);
        assert_eq!(store.len(), 1);
        // Unknown swarms answer empty, total.
        assert_eq!(store.peers(&[0xFF; 20]), []);
    }

    #[test]
    fn the_per_swarm_cap_truncates() {
        let mut store = PeerStore::new();
        for port in 0..u16::try_from(MAX_PEERS_PER_INFOHASH + 8).unwrap_or(u16::MAX) {
            store.announce(&[0x02; 20], localhost(port));
        }
        assert_eq!(store.peers(&[0x02; 20]).len(), MAX_PEERS_PER_INFOHASH);
    }

    #[test]
    fn forget_drops_a_contact_from_every_swarm() {
        let mut store = PeerStore::new();
        store.announce(
            &[0x03; 20],
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 6_881)),
        );
        let contact = Contact {
            id: crate::node_id::NodeId::from_bytes([0; 20]),
            addr: localhost(6_881),
        };
        store.forget(&contact);
        assert!(store.is_empty(), "the announced endpoint is gone");
    }
}
