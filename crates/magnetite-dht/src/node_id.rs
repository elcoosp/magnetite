//! Node identity for the DHT: the 160-bit [`NodeId`] and its XOR metric.
//!
//! Kademlia's one primitive is the XOR of two node ids, read as a 160-bit
//! unsigned integer: that value is the *distance* between the nodes, and
//! the position of its highest set bit picks the K-bucket a contact lives
//! in. This module keeps both operations total and allocation-free — the
//! distance is the XOR of the raw 20 bytes, big-endian, so comparing
//! distances is plain byte-slice ordering.

use std::fmt;

/// A Kademlia node id: 160 bits, the same width as a `BitTorrent` infohash
/// but a different concept — BEP 5 node ids are chosen by their distance
/// to other ids, not hashed from file bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId([u8; 20]);

impl NodeId {
    /// Wraps 20 raw id bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// The raw 20 id bytes, wire order.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// The Kademlia XOR distance between the two ids, big-endian: comparing
    /// these arrays is comparing the numeric 160-bit distances.
    #[must_use]
    pub fn distance(&self, other: &Self) -> [u8; 20] {
        let mut out = [0_u8; 20];
        for (index, pair) in self.0.iter().zip(other.0.iter()).enumerate() {
            out[index] = pair.0 ^ pair.1;
        }
        out
    }

    /// The number of leading bits the two ids share, from the most
    /// significant bit down; `160` means the ids are equal.
    #[must_use]
    pub fn shared_prefix_bits(&self, other: &Self) -> u32 {
        let xor = self.distance(other);
        let leading_zero_bytes = xor.iter().take_while(|byte| **byte == 0).count();
        if leading_zero_bytes == xor.len() {
            return 160;
        }
        let first = xor[leading_zero_bytes];
        let byte_offset = u32::try_from(leading_zero_bytes).unwrap_or(u32::MAX);
        byte_offset * 8 + first.leading_zeros()
    }

    /// The K-bucket index for a contact with id `other`: `None` when the
    /// ids are equal (our own id never goes into our own table), otherwise
    /// `0..=159` where `0` is the bucket of ids sharing 159 bits with ours
    /// (the closest half of the space) and `159` the bucket of ids sharing
    /// none (the far half).
    #[must_use]
    pub fn bucket_index(&self, other: &Self) -> Option<u8> {
        let shared = self.shared_prefix_bits(other);
        if shared == 160 {
            None
        } else {
            Some(u8::try_from(159 - shared).unwrap_or(159))
        }
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_is_xor_and_orders_by_bytes() {
        let a = NodeId::from_bytes([0x0F; 20]);
        let b = NodeId::from_bytes([0xF0; 20]);
        let distance = a.distance(&b);
        assert_eq!(distance[0], 0xFF);
        assert!(a.distance(&b) > a.distance(&a));
        assert_eq!(a.distance(&a), [0_u8; 20]);
    }

    #[test]
    fn shared_prefix_counts_leading_equal_bits() {
        let zeros = [0_u8; 20];
        let a = NodeId::from_bytes(zeros);
        // Differing in the last bit only: seven leading bits in common.
        let differs_last = NodeId::from_bytes([
            0b0000_0001,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        // Differing in the first bit: nothing in common.
        let differs_msb = NodeId::from_bytes([
            0b1000_0000,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        assert_eq!(a.shared_prefix_bits(&a), 160);
        assert_eq!(a.shared_prefix_bits(&differs_last), 7);
        assert_eq!(a.shared_prefix_bits(&differs_msb), 0);
    }

    #[test]
    fn bucket_index_spans_the_full_space() {
        let self_id = NodeId::from_bytes([0x80; 20]);
        // Differing in the very last bit: 159 shared bits -> bucket 0.
        let mut closest = self_id.as_bytes().to_owned();
        closest[19] ^= 1;
        assert_eq!(self_id.bucket_index(&NodeId::from_bytes(closest)), Some(0));
        // Differing in the first bit: 0 shared bits -> bucket 159.
        let mut farthest = self_id.as_bytes().to_owned();
        farthest[0] ^= 0x80;
        assert_eq!(
            self_id.bucket_index(&NodeId::from_bytes(farthest)),
            Some(159)
        );
        // Our own id has no bucket.
        assert_eq!(self_id.bucket_index(&self_id), None);
    }

    #[test]
    fn display_is_lowercase_hex() {
        let id = NodeId::from_bytes([
            0x00, 0xAB, 0xFF, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x7F,
        ]);
        assert_eq!(id.to_string(), "00abff010000000000000000000000000000007f");
    }
}
