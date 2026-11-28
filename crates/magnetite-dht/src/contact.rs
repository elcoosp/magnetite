//! Compact node-info entries (BEP 5) and the [`Contact`] they describe.
//!
//! BEP 5 ships node lists in a fixed-width compact form: 26 bytes per
//! `IPv4` entry (20 id bytes, 4 address bytes, 2 port bytes) and 38 bytes
//! per `IPv6` entry (20 id bytes, 16 address bytes, 2 port bytes). Like the
//! tracker's compact peer lists, parsing slices fixed-size entries straight
//! out of the caller's buffer and refuses ragged tails as typed errors —
//! nothing is allocated before the shape check passes.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use magnetite_core::error::{Error, Result};

use crate::node_id::NodeId;

/// Wire size of one compact `IPv4` node entry: 20 id + 4 address + 2 port.
pub const COMPACT_V4_LEN: usize = 26;

/// Wire size of one compact `IPv6` node entry: 20 id + 16 address + 2 port.
pub const COMPACT_V6_LEN: usize = 38;

/// One node in the DHT: its Kademlia id and the UDP endpoint that answers
/// for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// The node's 160-bit Kademlia id.
    pub id: NodeId,
    /// The UDP endpoint the node listens on.
    pub addr: SocketAddr,
}

/// Encodes one contact in the compact form matching its address family:
/// [`COMPACT_V4_LEN`] bytes for `IPv4`, [`COMPACT_V6_LEN`] for `IPv6`.
///
/// # Errors
/// Returns [`Error::Dht`] when the contact's address is neither a plain
/// IPv4 nor an IPv6 address (which `SocketAddr` cannot be, but the
/// result stays total for the same reason the encoders are).
pub fn encode_compact_node(contact: &Contact) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(COMPACT_V6_LEN);
    match contact.addr {
        SocketAddr::V4(v4) => {
            out.extend_from_slice(contact.id.as_bytes());
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            out.extend_from_slice(contact.id.as_bytes());
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    Ok(out)
}

/// Splits a BEP 5 `nodes` byte string into 26-byte `IPv4` contacts.
///
/// # Errors
/// Returns [`Error::Dht`] when the length is not a multiple of 26.
pub fn parse_compact_nodes_v4(bytes: &[u8]) -> Result<Vec<Contact>> {
    if bytes.len() % COMPACT_V4_LEN != 0 {
        return Err(Error::Dht(format!(
            "compact node list length {} is not a multiple of {COMPACT_V4_LEN}",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(COMPACT_V4_LEN)
        .map(|entry| {
            let mut id = [0_u8; 20];
            id.copy_from_slice(&entry[0..20]);
            Contact {
                id: NodeId::from_bytes(id),
                addr: SocketAddr::new(
                    std::net::IpAddr::V4(Ipv4Addr::new(entry[20], entry[21], entry[22], entry[23])),
                    u16::from_be_bytes([entry[24], entry[25]]),
                ),
            }
        })
        .collect())
}

/// Splits a BEP 5 `nodes6` byte string into 38-byte `IPv6` contacts.
///
/// # Errors
/// Returns [`Error::Dht`] when the length is not a multiple of 38.
pub fn parse_compact_nodes_v6(bytes: &[u8]) -> Result<Vec<Contact>> {
    if bytes.len() % COMPACT_V6_LEN != 0 {
        return Err(Error::Dht(format!(
            "compact node6 list length {} is not a multiple of {COMPACT_V6_LEN}",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(COMPACT_V6_LEN)
        .map(|entry| {
            let mut id = [0_u8; 20];
            id.copy_from_slice(&entry[0..20]);
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&entry[20..36]);
            Contact {
                id: NodeId::from_bytes(id),
                addr: SocketAddr::new(
                    std::net::IpAddr::V6(Ipv6Addr::from(octets)),
                    u16::from_be_bytes([entry[36], entry[37]]),
                ),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact_v4(port: u16) -> Contact {
        Contact {
            id: NodeId::from_bytes([0x5A; 20]),
            addr: SocketAddr::from(([192, 0, 2, 9], port)),
        }
    }

    #[test]
    fn encodes_v4_id_then_ip_then_port() {
        let wire = encode_compact_node(&contact_v4(6_881)).expect("encodes");
        assert_eq!(wire.len(), COMPACT_V4_LEN);
        assert_eq!(&wire[..20], &[0x5A; 20]);
        assert_eq!(&wire[20..24], &[192, 0, 2, 9]);
        assert_eq!(&wire[24..26], &6_881_u16.to_be_bytes());
    }

    #[test]
    fn encodes_v6_entries_at_thirty_eight_bytes() {
        let contact = Contact {
            id: NodeId::from_bytes([0x5A; 20]),
            addr: SocketAddr::new(
                Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 1).into(),
                6_881,
            ),
        };
        let wire = encode_compact_node(&contact).expect("encodes");
        assert_eq!(wire.len(), COMPACT_V6_LEN);
        assert_eq!(
            &wire[20..36],
            &Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 1).octets()
        );
    }

    #[test]
    fn parses_v4_lists_and_rejects_ragged_tails() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_compact_node(&contact_v4(6_881)).expect("encodes"));
        wire.extend_from_slice(&encode_compact_node(&contact_v4(6_882)).expect("encodes"));
        let parsed = parse_compact_nodes_v4(&wire).expect("parses");
        assert_eq!(parsed, vec![contact_v4(6_881), contact_v4(6_882)]);
        let err = parse_compact_nodes_v4(&wire[..25]).expect_err("ragged tail");
        assert!(err.to_string().contains("multiple of 26"), "got {err}");
    }

    #[test]
    fn empty_node_lists_are_valid() {
        assert_eq!(parse_compact_nodes_v4(&[]).expect("empty"), []);
        assert_eq!(parse_compact_nodes_v6(&[]).expect("empty"), []);
    }

    #[test]
    fn rejects_ragged_v6_lists() {
        assert!(parse_compact_nodes_v6(&[0_u8; 37]).is_err());
    }
}
