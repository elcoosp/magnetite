//! Property-based tests: generated values round-trip through the codec and
//! arbitrary input never panics — adversarial bytes are data, not crashes.

use std::borrow::Cow;

use magnetite_core::bencode::{self, Value};
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
}
