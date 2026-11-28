//! The Kademlia routing table: 160 K-buckets over the 160-bit id space.
//!
//! The bucket a contact belongs to is the position of the highest bit it
//! differs on from our own id ([`NodeId::bucket_index`]) — bucket `0` holds
//! the contacts sharing 159 bits with us (the rarest, most valuable half of
//! the space), bucket `159` the ones sharing none. Each bucket keeps at
//! most [`KBUCKET_SIZE`] contacts, most recently seen first (the LRU order
//! Kademlia refreshes against), and re-seeing a known id moves it back to
//! the front instead of duplicating it.
//!
//! Eviction is deliberately *not* automatic: a full bucket answers
//! [`InsertOutcome::BucketFull`] with the candidate it rejected, so the
//! caller — the future refresh logic — decides whether to probe the
//! stalest contact first. That is the same "the caller owns the policy"
//! seam the endgame board and the upload policy shipped with.

use crate::contact::Contact;
use crate::node_id::NodeId;

/// Contacts per K-bucket: BEP 5 practice (`k` in Kademlia terms), small
/// enough that a bucket fits in one UDP reply's worth of node info.
pub const KBUCKET_SIZE: usize = 8;

/// What [`RoutingTable::insert`] did with the contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// A new contact entered the table.
    Inserted,
    /// A known contact was refreshed (moved to the front of its bucket);
    /// the contact's id was already in the table.
    Refreshed(Contact),
    /// The bucket is at [`KBUCKET_SIZE`] and the candidate was refused; the
    /// caller decides whether to probe-and-evict (the documented policy
    /// seam this groundwork leaves to the refresh logic).
    BucketFull(Contact),
}

/// The K-buckets over the 160-bit id space, keyed by our own node id.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    self_id: NodeId,
    buckets: [Vec<Contact>; 160],
}

impl RoutingTable {
    /// A table rooted at `self_id`; our own id is never stored.
    #[must_use]
    pub fn new(self_id: NodeId) -> Self {
        Self {
            self_id,
            buckets: std::array::from_fn(|_| Vec::new()),
        }
    }

    /// The id the buckets are keyed against.
    #[must_use]
    pub fn self_id(&self) -> &NodeId {
        &self.self_id
    }

    /// Inserts or refreshes a contact. Inserting our own id is a no-op that
    /// answers [`InsertOutcome::Refreshed`] with it — the table never holds
    /// itself.
    pub fn insert(&mut self, contact: Contact) -> InsertOutcome {
        if contact.id == self.self_id {
            return InsertOutcome::Refreshed(contact);
        }
        let Some(bucket) = self
            .self_id
            .bucket_index(&contact.id)
            .and_then(|index| self.buckets.get_mut(usize::from(index)))
        else {
            return InsertOutcome::Refreshed(contact);
        };
        // Known id: refresh in place, moving the contact to the front of
        // the bucket's recency order.
        if let Some(existing) = bucket
            .iter_mut()
            .find(|candidate| candidate.id == contact.id)
        {
            existing.addr = contact.addr;
            let refreshed = existing.clone();
            bucket.retain(|candidate| candidate.id != contact.id);
            bucket.insert(0, refreshed.clone());
            return InsertOutcome::Refreshed(refreshed);
        }
        if bucket.len() >= KBUCKET_SIZE {
            return InsertOutcome::BucketFull(contact);
        }
        bucket.insert(0, contact);
        InsertOutcome::Inserted
    }

    /// The up-to-`count` contacts closest to `target` by XOR distance,
    /// scarcest distance first, ties broken by id order — the same shape a
    /// `find_node` reply and the future iterative lookup both need.
    #[must_use]
    pub fn closest_to(&self, target: &NodeId, count: usize) -> Vec<Contact> {
        let mut all: Vec<Contact> = self
            .buckets
            .iter()
            .flat_map(|bucket| bucket.iter().cloned())
            .collect();
        all.sort_by(|left, right| {
            left.id
                .distance(target)
                .cmp(&right.id.distance(target))
                .then_with(|| left.id.cmp(&right.id))
        });
        all.truncate(count);
        all
    }

    /// How many contacts the table holds across all buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    /// Whether the table holds no contacts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Contacts currently in one bucket (`0..=159`).
    #[must_use]
    pub fn bucket_len(&self, index: u8) -> usize {
        self.buckets.get(usize::from(index)).map_or(0, Vec::len)
    }

    /// How many buckets hold at least one contact — a refresh scheduler's
    /// measure of spread.
    #[must_use]
    pub fn occupied_buckets(&self) -> usize {
        self.buckets
            .iter()
            .filter(|bucket| !bucket.is_empty())
            .count()
    }

    /// Every contact, bucket order (bucket `0` first) — the bootstrap and
    /// persistence paths' snapshot.
    #[must_use]
    pub fn all_contacts(&self) -> Vec<Contact> {
        self.buckets
            .iter()
            .flat_map(|bucket| bucket.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(byte: u8, port: u16) -> Contact {
        Contact {
            id: NodeId::from_bytes([byte; 20]),
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    #[test]
    fn inserts_go_to_the_right_bucket() {
        let self_id = NodeId::from_bytes([0x80; 20]);
        let mut table = RoutingTable::new(self_id);
        // Same first byte differs in the second: high bucket.
        assert_eq!(table.insert(contact(0x81, 1)), InsertOutcome::Inserted);
        let mut far = [0_u8; 20];
        far[0] = 0x00;
        assert_eq!(
            table.insert(Contact {
                id: NodeId::from_bytes(far),
                addr: std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
            }),
            InsertOutcome::Inserted
        );
        assert_eq!(table.len(), 2);
        assert_eq!(table.occupied_buckets(), 2);
        // Bucket index of a contact differing in bit 120-ish lands in a
        // single bucket with one contact each.
        assert_eq!(table.bucket_len(159), 1, "the far contact");
    }

    #[test]
    fn inserting_our_own_id_never_stores_it() {
        let self_id = NodeId::from_bytes([0x42; 20]);
        let mut table = RoutingTable::new(self_id);
        assert_eq!(
            table.insert(contact(0x42, 1)),
            InsertOutcome::Refreshed(Contact {
                id: self_id,
                addr: std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            })
        );
        assert!(table.is_empty());
    }

    #[test]
    fn re_seeing_a_known_id_refreshes_and_updates_the_address() {
        let mut table = RoutingTable::new(NodeId::from_bytes([0x10; 20]));
        table.insert(contact(0x11, 1));
        table.insert(contact(0x12, 2));
        let moved = Contact {
            id: NodeId::from_bytes([0x11; 20]),
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
        };
        assert_eq!(table.insert(moved.clone()), InsertOutcome::Refreshed(moved));
        let snapshot = table.all_contacts();
        assert_eq!(snapshot[0].addr.port(), 9, "refreshed contact is first");
        assert_eq!(snapshot.len(), 2, "no duplicate");
    }

    #[test]
    fn buckets_fill_at_k_and_report_the_rejected_candidate() {
        // Our id is all zeros; contacts differing only in the last byte,
        // with that byte's top bit set, all land in one bucket (they share
        // 152 bits with us, so bucket 159 - 152 - 0 = 7).
        let mut table = RoutingTable::new(NodeId::from_bytes([0x00; 20]));
        for i in 0..KBUCKET_SIZE {
            let mut bytes = [0_u8; 20];
            bytes[19] = 128 + u8::try_from(i).unwrap_or(0);
            assert_eq!(
                table.insert(Contact {
                    id: NodeId::from_bytes(bytes),
                    addr: std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        u16::try_from(i + 1).unwrap_or(1)
                    )),
                }),
                InsertOutcome::Inserted,
                "contact {i} enters bucket 7"
            );
        }
        assert_eq!(table.bucket_len(7), KBUCKET_SIZE);
        let overflow = Contact {
            id: NodeId::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 200]),
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], 99)),
        };
        assert_eq!(
            table.insert(overflow.clone()),
            InsertOutcome::BucketFull(overflow),
            "the candidate is handed back for the probe-and-evict policy"
        );
    }

    #[test]
    fn closest_to_ranks_by_xor_distance() {
        let mut table = RoutingTable::new(NodeId::from_bytes([0x00; 20]));
        for byte in [0x80_u8, 0x40, 0x01, 0x7F] {
            table.insert(contact(byte, u16::from(byte)));
        }
        let target = NodeId::from_bytes([0x7E; 20]);
        let closest = table.closest_to(&target, 2);
        assert_eq!(closest[0].id.as_bytes()[0], 0x7F, "distance 1");
        assert_eq!(closest[1].id.as_bytes()[0], 0x40, "next scarcest");
    }

    #[test]
    fn empty_table_answers_no_contacts() {
        let table = RoutingTable::new(NodeId::from_bytes([0; 20]));
        assert!(table.is_empty());
        assert_eq!(table.closest_to(&NodeId::from_bytes([1; 20]), 8), []);
    }
}
