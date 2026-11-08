//! Property-based tests: generated values round-trip through the codec and
//! arbitrary input never panics — adversarial bytes are data, not crashes.

use std::borrow::Cow;

use magnetite_core::bencode::{self, Value};
use magnetite_core::tracker;
use proptest::prelude::*;

fn arb_bytes() -> impl Strategy<Value = Cow<'static, [u8]>> {
    proptest::collection::vec(any::<u8>(), 0..64).prop_map(Cow::Owned)
}

fn arb_value() -> impl Strategy<Value = Value<'static>> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(Value::Int),
        arb_bytes().prop_map(Value::Bytes),
    ];
    leaf.prop_recursive(4, 32, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::List),
            proptest::collection::vec((arb_bytes(), inner), 0..8)
                .prop_map(|pairs| Value::Dict(pairs.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn round_trips_through_bencode(v in arb_value()) {
        let encoded = bencode::encode_to_vec(&v).unwrap();
        let decoded = bencode::decode(&encoded).unwrap();
        prop_assert_eq!(decoded, v);
    }

    #[test]
    fn arbitrary_input_never_panics(input in proptest::collection::vec(any::<u8>(), 0..256)) {
        // May succeed or fail, but must never panic, hang, or over-allocate:
        // allocation is bounded by input length by construction.
        let _ = bencode::decode(&input);
    }

    #[test]
    fn truncated_input_never_panics(input in proptest::collection::vec(any::<u8>(), 1..128)) {
        let truncated = &input[..input.len() - 1];
        let _ = bencode::decode(truncated);
    }

    #[test]
    fn entry_spans_address_the_raw_values(v in arb_value()) {
        let encoded = bencode::encode_to_vec(&v).unwrap();
        let (decoded, spans) = bencode::decode_with_entry_spans(&encoded).unwrap();
        prop_assert_eq!(&decoded, &v);
        match &decoded {
            Value::Dict(map) => {
                prop_assert_eq!(spans.iter().count(), map.len());
                for (key, span) in spans.iter() {
                    // Each span must address the exact raw bytes that decode
                    // back into the value stored under its key.
                    let inner = bencode::decode(&encoded[span]).unwrap();
                    prop_assert_eq!(map.get(key), Some(&inner));
                }
            }
            // Only top-level dictionaries carry entry spans.
            _ => prop_assert!(spans.is_empty()),
        }
    }

    #[test]
    fn spanned_decode_agrees_with_plain_decode(
        input in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let plain = bencode::decode(&input);
        let spanned = bencode::decode_with_entry_spans(&input).map(|(v, _)| v);
        prop_assert_eq!(plain.is_ok(), spanned.is_ok());
        if let (Ok(plain_value), Ok(spanned_value)) = (plain, spanned) {
            prop_assert_eq!(plain_value, spanned_value);
        }
    }

    #[test]
    fn tracker_packets_never_panic(input in proptest::collection::vec(any::<u8>(), 0..256)) {
        // Tracker datagrams are just as adversarial as bencode: every
        // parser must answer with a typed error, never a panic.
        let transaction_id = 0x1234_5678_u32;
        let _ = tracker::parse_connect_response(transaction_id, &input);
        let _ = tracker::parse_announce_response(transaction_id, &input);
        let _ = tracker::parse_compact_peers_v4(&input);
        let _ = tracker::parse_compact_peers_v6(&input);
    }

    #[test]
    fn connect_responses_round_trip(
        transaction_id in any::<u32>(),
        connection_id in any::<i64>(),
    ) {
        let mut packet = Vec::new();
        packet.extend_from_slice(&0_u32.to_be_bytes());
        packet.extend_from_slice(&transaction_id.to_be_bytes());
        packet.extend_from_slice(&connection_id.to_be_bytes());
        prop_assert_eq!(
            tracker::parse_connect_response(transaction_id, &packet)?,
            connection_id
        );
    }

    #[test]
    fn announce_responses_round_trip(
        transaction_id in any::<u32>(),
        interval in any::<u32>(),
        leechers in any::<u32>(),
        seeders in any::<u32>(),
        raw_peers in proptest::collection::vec(any::<u8>(), 0..60),
    ) {
        let peers_len = raw_peers.len() - raw_peers.len() % 6;
        let mut packet = Vec::new();
        packet.extend_from_slice(&1_u32.to_be_bytes());
        packet.extend_from_slice(&transaction_id.to_be_bytes());
        packet.extend_from_slice(&interval.to_be_bytes());
        packet.extend_from_slice(&leechers.to_be_bytes());
        packet.extend_from_slice(&seeders.to_be_bytes());
        packet.extend_from_slice(&raw_peers[..peers_len]);

        let parsed = tracker::parse_announce_response(transaction_id, &packet)?;
        prop_assert_eq!(parsed.interval, interval);
        prop_assert_eq!(parsed.leechers, leechers);
        prop_assert_eq!(parsed.seeders, seeders);
        prop_assert_eq!(parsed.peers.len(), peers_len / 6);
    }
}
