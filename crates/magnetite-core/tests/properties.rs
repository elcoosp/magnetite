//! Property-based tests: generated values round-trip through the codec and
//! arbitrary input never panics — adversarial bytes are data, not crashes.

use std::borrow::Cow;

use magnetite_core::bencode::{self, Value};
use magnetite_core::downloader::{
    Availability, BlockAssembler, BlockPlan, PieceField, PiecePicker, PieceStrategy,
};
use magnetite_core::infohash::InfoHash;
use magnetite_core::peer_wire::{self, Handshake, Message};
use magnetite_core::tracker;
use magnetite_core::verify::PieceVerifier;
use proptest::prelude::*;
use sha1::{Digest, Sha1};

fn arb_bytes() -> impl Strategy<Value = Cow<'static, [u8]>> {
    proptest::collection::vec(any::<u8>(), 0..64).prop_map(Cow::Owned)
}

fn arb_byte_array20() -> impl Strategy<Value = [u8; 20]> {
    proptest::collection::vec(any::<u8>(), 20).prop_map(|bytes| {
        let mut array = [0_u8; 20];
        array.copy_from_slice(&bytes);
        array
    })
}

fn arb_peer_message() -> impl Strategy<Value = Message<'static>> {
    prop_oneof![
        Just(Message::KeepAlive),
        Just(Message::Choke),
        Just(Message::Unchoke),
        Just(Message::Interested),
        Just(Message::NotInterested),
        any::<u32>().prop_map(Message::Have),
        arb_bytes().prop_map(Message::Bitfield),
        (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(|(index, begin, length)| {
            Message::Request {
                index,
                begin,
                length,
            }
        }),
        (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(|(index, begin, length)| {
            Message::Cancel {
                index,
                begin,
                length,
            }
        }),
        any::<u16>().prop_map(Message::Port),
        (any::<u32>(), any::<u32>(), arb_bytes()).prop_map(|(index, begin, block)| {
            Message::Piece {
                index,
                begin,
                block,
            }
        }),
        (10_u8.., arb_bytes()).prop_map(|(id, payload)| Message::Unknown { id, payload }),
    ]
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

    #[test]
    fn peer_wire_messages_round_trip(message in arb_peer_message()) {
        let encoded = message.encode();
        let parsed = peer_wire::parse_message(&encoded)?;
        prop_assert_eq!(parsed.into_owned(), message);
    }

    #[test]
    fn peer_wire_frames_never_panic(input in proptest::collection::vec(any::<u8>(), 0..256)) {
        // Peer wire framing is just as adversarial as bencode and tracker
        // datagrams: every entry point answers with a typed error, never a
        // panic, and a hostile length prefix is rejected before slicing.
        let _ = peer_wire::next_message(&input);
        let _ = peer_wire::parse_message(&input);
    }

    #[test]
    fn handshakes_round_trip(
        info_bytes in arb_byte_array20(),
        peer_bytes in arb_byte_array20(),
        reserved in proptest::collection::vec(any::<u8>(), 8),
    ) {
        let mut handshake = Handshake::new(InfoHash::from_bytes(info_bytes), peer_bytes);
        handshake.reserved.copy_from_slice(&reserved);
        let encoded = handshake.encode();
        prop_assert_eq!(peer_wire::Handshake::parse(&encoded)?, handshake);
    }

    #[test]
    fn handshake_inputs_never_panic(input in proptest::collection::vec(any::<u8>(), 0..128)) {
        let _ = peer_wire::Handshake::parse(&input);
        let _ = peer_wire::Handshake::parse_prefix(&input);
    }

    // --- downloader: piece fields, picker, assembler -----------------------

    /// A piece field built bit-by-bit round-trips through the BEP 3 wire form.
    #[test]
    fn piece_fields_round_trip(
        piece_count in 1_u32..=256_u32,
        have in proptest::collection::vec(any::<bool>(), 0..256),
    ) {
        let mut field = PieceField::new(piece_count).unwrap();
        for index in 0..piece_count {
            let flag = have
                .get(index as usize)
                .copied()
                .unwrap_or(false);
            if flag {
                field.set_have(index).unwrap();
            }
        }
        let wire = field.to_bitfield();
        prop_assert_eq!(wire.len(), piece_count.div_ceil(8) as usize);
        let reparsed = PieceField::from_bitfield(&wire, piece_count).unwrap();
        for index in 0..piece_count {
            prop_assert_eq!(reparsed.has(index), field.has(index));
        }
        prop_assert_eq!(reparsed.have_count(), field.have_count());
        prop_assert_eq!(reparsed, field);
    }

    /// Arbitrary bytes as a bitfield answer with a value or a typed error,
    /// never a panic, and every accepted bitfield re-encodes canonically.
    #[test]
    fn bitfield_parsing_never_panics(
        piece_count in 0_u32..=64_u32,
        bytes in proptest::collection::vec(any::<u8>(), 0..24),
    ) {
        let parsed = PieceField::from_bitfield(&bytes, piece_count);
        if piece_count == 0 {
            prop_assert!(parsed.is_err());
            return Ok(());
        }
        if let Ok(field) = parsed {
            let canonical = field.to_bitfield();
            let again = PieceField::from_bitfield(&canonical, piece_count).unwrap();
            prop_assert_eq!(again, field);
        }
    }

    /// The picker only ever names a piece the peer has and we miss; with
    /// availability it names one of the scarcest candidates, and
    /// `add_peer`/`remove_peer` mirror `update`/saturating decrements.
    #[test]
    fn picker_selects_only_pieces_the_peer_has_and_we_miss(
        piece_count in 1_u32..=48_u32,
        ours_mask in proptest::collection::vec(any::<bool>(), 0..48),
        theirs_mask in proptest::collection::vec(any::<bool>(), 0..48),
    ) {
        let mut ours = PieceField::new(piece_count).unwrap();
        let mut theirs = PieceField::new(piece_count).unwrap();
        let mut availability = Availability::new(piece_count).unwrap();
        for index in 0..piece_count {
            if ours_mask.get(index as usize).copied().unwrap_or(false) {
                ours.set_have(index).unwrap();
            }
            if theirs_mask.get(index as usize).copied().unwrap_or(false) {
                theirs.set_have(index).unwrap();
                availability.update(index).unwrap();
            }
        }
        // add_peer/remove_peer are exactly update/saturating-decrement.
        let mut mirrored = Availability::new(piece_count).unwrap();
        mirrored.add_peer(&theirs);
        for index in 0..piece_count {
            prop_assert_eq!(mirrored.count(index), availability.count(index));
        }
        mirrored.remove_peer(&theirs);
        for index in 0..piece_count {
            prop_assert_eq!(mirrored.count(index), 0);
        }

        let sequential = PiecePicker::new(PieceStrategy::Sequential);
        if let Some(picked) = sequential.pick(&ours, &theirs, None) {
            prop_assert!(theirs.has(picked));
            prop_assert!(!ours.has(picked));
            let lowest = (0..piece_count)
                .filter(|&index| theirs.has(index) && !ours.has(index))
                .min();
            prop_assert_eq!(Some(picked), lowest);
        }

        let rarest = PiecePicker::new(PieceStrategy::RarestFirst);
        let candidates = (0..piece_count).filter(|&index| theirs.has(index) && !ours.has(index));
        if let Some(picked) = rarest.pick(&ours, &theirs, Some(&availability)) {
            prop_assert!(theirs.has(picked));
            prop_assert!(!ours.has(picked));
            let scarcest = candidates.map(|index| availability.count(index)).min();
            prop_assert_eq!(Some(availability.count(picked)), scarcest);
        } else {
            prop_assert!(candidates.count() == 0);
        }
    }

    /// Hostile (index, begin, block) triples never panic the assembler.
    #[test]
    fn assembler_pushes_never_panic(
        index in any::<u32>(),
        begin in any::<u32>(),
        block in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut assembler = BlockAssembler::new(4, 33_000, 132_000, 4).unwrap();
        let _ = assembler.push_block(index, begin, &block);
    }

    /// Every valid layout completes through the assembler in any block
    /// order: each piece is emitted exactly once with the exact bytes fed
    /// in, and after completion re-planning is a typed error.
    #[test]
    fn assembler_completes_any_layout_in_any_block_order(
        piece_count in 1_u32..=16_u32,
        piece_length in 1_u32..=48_000_u32,
        last_length in 1_u32..=48_000_u32,
        order_keys in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let last_length = last_length.min(piece_length);
        let total =
            u64::from(piece_count - 1) * u64::from(piece_length) + u64::from(last_length);
        let mut assembler = BlockAssembler::new(piece_count, piece_length, total, piece_count)
            .unwrap();

        // Deterministic filler pattern: piece `i`, offset `o` -> (7i + o).
        let expected: Vec<Vec<u8>> = (0..piece_count)
            .map(|piece| {
                let length = if piece == piece_count - 1 {
                    last_length
                } else {
                    piece_length
                };
                (0..length)
                    .map(|offset| {
                        u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251)
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .collect();

        let plans: Vec<BlockPlan> = (0..piece_count)
            .map(|piece| assembler.block_plan(piece).unwrap())
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect();
        // Permute the delivery order with the generated keys (cycled).
        let mut delivered: Vec<(u8, BlockPlan)> = plans
            .into_iter()
            .enumerate()
            .map(|(position, plan)| {
                let key = order_keys
                    .get(position % order_keys.len().max(1))
                    .copied()
                    .unwrap_or(0);
                (key, plan)
            })
            .collect();
        delivered.sort_by(|(key_a, plan_a), (key_b, plan_b)| {
            (key_a, plan_a.index, plan_a.begin).cmp(&(key_b, plan_b.index, plan_b.begin))
        });

        let mut completions = 0_u32;
        for (_key, plan) in &delivered {
            let block = &expected[plan.index as usize]
                [plan.begin as usize..(plan.begin + plan.length) as usize];
            if let Some((index, piece)) =
                assembler.push_block(plan.index, plan.begin, block).unwrap()
            {
                prop_assert_eq!(index, plan.index);
                prop_assert_eq!(&piece, &expected[plan.index as usize]);
                completions += 1;
            }
        }
        prop_assert_eq!(completions, piece_count);
        prop_assert_eq!(assembler.pieces_completed(), piece_count);
        prop_assert!(assembler.block_plan(0).is_err());
    }

    // --- verify: piece verification -----------------------------------------

    /// A download streamed through the assembler verifies against hashes
    /// computed independently in the test (never via the code under test),
    /// a corrupted piece fails as data (Ok(false), nothing marked), and the
    /// re-downloaded piece then verifies — the exact retry loop a session
    /// needs.
    #[test]
    fn assembled_pieces_verify_against_independent_hashes(
        piece_count in 1_u32..=12_u32,
        piece_length in 1_u32..=9_000_u32,
        corrupt_index in 0_u32..=12_u32,
        corrupt_byte in any::<u8>(),
    ) {
        // Deterministic filler pattern, same shape as the assembler spec.
        let pieces: Vec<Vec<u8>> = (0..piece_count)
            .map(|piece| {
                (0..piece_length)
                    .map(|offset| {
                        u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251)
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .collect();
        // Independent SHA-1 oracle over the expected bytes.
        let mut hashes = Vec::new();
        for piece in &pieces {
            hashes.extend_from_slice(&Sha1::digest(piece));
        }

        let total = u64::from(piece_count) * u64::from(piece_length);
        let mut assembler =
            BlockAssembler::new(piece_count, piece_length, total, piece_count.clamp(1, 8))
                .unwrap();
        let mut verifier = PieceVerifier::new(&hashes, piece_count).unwrap();
        prop_assert_eq!(verifier.verified_count(), 0_u32);

        for index in 0..piece_count {
            let piece = &pieces[index as usize];
            for plan in assembler.block_plan(index).unwrap() {
                let block = &piece[plan.begin as usize..(plan.begin + plan.length) as usize];
                if let Some((completed, bytes)) =
                    assembler.push_block(plan.index, plan.begin, block).unwrap()
                {
                    prop_assert_eq!(completed, index);
                    // Corrupt exactly one byte of one emitted piece.
                    let mut bytes = bytes;
                    if index == corrupt_index % piece_count {
                        let last = bytes.len() - 1;
                        bytes[last] = bytes[last].wrapping_add(corrupt_byte | 1);
                    }
                    let ok = verifier.verify_piece(index, &bytes).unwrap();
                    prop_assert_eq!(ok, index != corrupt_index % piece_count);
                }
            }
        }

        // The corrupted piece stayed unverified and retryable; a mismatch is
        // data, so re-verifying the true bytes succeeds and completes.
        let corrupt = corrupt_index % piece_count;
        prop_assert_eq!(verifier.verified_count(), piece_count - 1);
        prop_assert!(!verifier.is_complete());
        let ok = verifier.verify_piece(corrupt, &pieces[corrupt as usize]).unwrap();
        prop_assert!(ok);
        prop_assert!(verifier.is_complete());
        prop_assert_eq!(verifier.verified_count(), piece_count);
        prop_assert_eq!(verifier.verified_field().have_count(), piece_count);
        // Verified pieces are closed for business, like assembled ones.
        prop_assert!(verifier.verify_piece(corrupt, &pieces[corrupt as usize]).is_err());
    }

    /// Hostile (hash-slice, index, bytes) triples never panic the verifier:
    /// every input answers with a value or a typed error.
    #[test]
    fn verification_never_panics(
        hashes in proptest::collection::vec(any::<u8>(), 0..80),
        piece_count in any::<u32>(),
        index in any::<u32>(),
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        if let Ok(mut verifier) = PieceVerifier::new(&hashes, piece_count) {
            let _ = verifier.verify_piece(index, &bytes);
            let _ = verifier.expected_hash(index);
            let _ = verifier.piece_count();
            let _ = verifier.verified_count();
            let _ = verifier.is_complete();
        }
    }

    /// Every accepted hash slice round-trips: `expected_hash` returns the
    /// exact 20 bytes that went in, per piece, in order.
    #[test]
    fn expected_hashes_address_the_raw_entries(
        count in 1_u32..=16_u32,
        seed in any::<u8>(),
    ) {
        let hashes: Vec<u8> = (0..count as usize * 20)
            .map(|position| seed.wrapping_add(u8::try_from(position % 256).unwrap()))
            .collect();
        let verifier = PieceVerifier::new(&hashes, count).unwrap();
        for index in 0..count {
            let start = index as usize * 20;
            prop_assert_eq!(verifier.expected_hash(index).unwrap(), &hashes[start..start + 20]);
        }
        prop_assert!(verifier.expected_hash(count).is_err());
    }
}

proptest! {
    /// Arbitrary message streams never panic the session router: hostile
    /// bitfields, out-of-range haves, malformed blocks, and spurious
    /// cancels are typed errors — data, not crashes.
    #[test]
    fn session_routes_never_panic(messages in proptest::collection::vec(arb_peer_message(), 0..48)) {
        use magnetite_core::session::SessionPump;

        let hashes: Vec<u8> = (0..4 * 20)
            .map(|position| u8::try_from(position).unwrap_or(0))
            .collect();
        let mut pump =
            SessionPump::new(&hashes, 4, 33_000, 132_000, 4, 8, PieceStrategy::RarestFirst)
                .unwrap();
        for message in &messages {
            let _ = pump.handle_message(message);
            let _ = pump.next_request();
        }
    }

    /// Every valid layout completes through the session pump: the pump
    /// plans requests under the pipeline cap, serves each block exactly
    /// once, verifies every piece against the metainfo digests, and marks
    /// nothing rejected.
    #[test]
    fn session_downloads_any_layout_in_any_request_order(
        piece_count in 1_u32..=8_u32,
        piece_length in 1_u32..=48_000_u32,
        last_length in 1_u32..=48_000_u32,
    ) {
        use magnetite_core::session::SessionPump;

        let last_length = last_length.min(piece_length);
        let total =
            u64::from(piece_count - 1) * u64::from(piece_length) + u64::from(last_length);

        // Deterministic filler pattern: piece `i`, offset `o` -> (7i + o),
        // the same generator the assembler specs verify against.
        let pieces: Vec<Vec<u8>> = (0..piece_count)
            .map(|piece| {
                let length = if piece == piece_count - 1 {
                    last_length
                } else {
                    piece_length
                };
                (0..length)
                    .map(|offset| {
                        u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251)
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .collect();
        let hashes: Vec<Vec<u8>> = pieces
            .iter()
            .map(|piece| Sha1::digest(piece).to_vec())
            .collect();
        let flat_hashes: Vec<u8> = hashes.concat();

        let mut pump = SessionPump::new(
            &flat_hashes,
            piece_count,
            piece_length,
            total,
            piece_count,
            250,
            PieceStrategy::RarestFirst,
        )
        .unwrap();
        let mut wire = PieceField::new(piece_count).unwrap();
        for index in 0..piece_count {
            wire.set_have(index).unwrap();
        }
        let event = pump
            .handle_message(&Message::Bitfield(Cow::Owned(wire.to_bitfield())))
            .unwrap();
        prop_assert_eq!(event, magnetite_core::session::SessionEvent::PeerBitfield {
            have_count: piece_count,
        });

        let mut verified_bytes: Vec<u8> = Vec::new();
        while let Some(plan) = pump.next_request().unwrap() {
            let end = (plan.begin + plan.length) as usize;
            let block = pieces[plan.index as usize][plan.begin as usize..end].to_vec();
            let event = pump
                .handle_message(&Message::Piece {
                    index: plan.index,
                    begin: plan.begin,
                    block: Cow::Owned(block),
                })
                .unwrap();
            match event {
                magnetite_core::session::SessionEvent::PieceVerified { index, bytes } => {
                    prop_assert_eq!(&bytes, &pieces[index as usize]);
                    verified_bytes.extend_from_slice(&bytes);
                }
                magnetite_core::session::SessionEvent::BlockBuffered { .. } => {}
                other => prop_assert!(false, "unexpected event {other:?}"),
            }
        }
        prop_assert!(pump.is_complete());
        prop_assert!(pump.rejected().is_empty());
        prop_assert_eq!(verified_bytes.len() as u64, total);
        prop_assert_eq!(pump.verified_count(), piece_count);
        prop_assert!(pump.pipeline().is_empty());
    }

    /// One corrupt delivery is not the end of the download: the rejected
    /// piece is requeued, the clean retry verifies, and the torrent
    /// completes exactly once — for any layout and any corrupted piece.
    #[test]
    fn session_completes_after_requeueing_a_corrupt_piece(
        piece_count in 1_u32..=6_u32,
        piece_length in 1_u32..=24_000_u32,
        corrupt_index in 0_u32..=6_u32,
    ) {
        use magnetite_core::session::SessionPump;

        // Deterministic filler pattern: piece `i`, offset `o` -> (7i + o).
        let pieces: Vec<Vec<u8>> = (0..piece_count)
            .map(|piece| {
                (0..piece_length)
                    .map(|offset| {
                        u8::try_from((u64::from(piece) * 7 + u64::from(offset)) % 251)
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .collect();
        let hashes: Vec<u8> = pieces
            .iter()
            .map(|piece| Sha1::digest(piece).to_vec())
            .collect::<Vec<_>>()
            .concat();
        let total = u64::from(piece_count) * u64::from(piece_length);
        let mut pump = SessionPump::new(
            &hashes,
            piece_count,
            piece_length,
            total,
            piece_count,
            250,
            PieceStrategy::RarestFirst,
        )
        .unwrap();
        let mut wire = PieceField::new(piece_count).unwrap();
        for index in 0..piece_count {
            wire.set_have(index).unwrap();
        }
        pump.handle_message(&Message::Bitfield(Cow::Owned(wire.to_bitfield())))
            .unwrap();

        // Corrupt exactly one delivery: its piece must come back rejected,
        // never verified, and stay listed for the requeue loop.
        let bad = corrupt_index % piece_count;
        let mut corrupt_served = false;
        let mut completed: Vec<u32> = Vec::new();
        while let Some(plan) = pump.next_request().unwrap() {
            let end = (plan.begin + plan.length) as usize;
            let mut block = pieces[plan.index as usize][plan.begin as usize..end].to_vec();
            if plan.index == bad && !corrupt_served {
                block[0] = block[0].wrapping_add(1);
                corrupt_served = true;
            }
            match pump
                .handle_message(&Message::Piece {
                    index: plan.index,
                    begin: plan.begin,
                    block: Cow::Owned(block),
                })
                .unwrap()
            {
                magnetite_core::session::SessionEvent::PieceVerified { index, bytes } => {
                    prop_assert_eq!(&bytes, &pieces[index as usize]);
                    completed.push(index);
                }
                magnetite_core::session::SessionEvent::PieceRejected { index } => {
                    prop_assert_eq!(index, bad);
                }
                magnetite_core::session::SessionEvent::BlockBuffered { .. } => {}
                other => prop_assert!(false, "unexpected event {other:?}"),
            }
        }
        prop_assert!(pump.rejected().contains(&bad));

        // The requeue loop reopens every rejected piece; the clean retry
        // completes the torrent with every piece verified exactly once.
        let rejected_before: Vec<u32> = pump.rejected().to_vec();
        let requeued = pump.retry_rejected().unwrap();
        prop_assert_eq!(requeued, rejected_before);
        prop_assert!(pump.rejected().is_empty());
        while let Some(plan) = pump.next_request().unwrap() {
            let end = (plan.begin + plan.length) as usize;
            let block = pieces[plan.index as usize][plan.begin as usize..end].to_vec();
            match pump
                .handle_message(&Message::Piece {
                    index: plan.index,
                    begin: plan.begin,
                    block: Cow::Owned(block),
                })
                .unwrap()
            {
                magnetite_core::session::SessionEvent::PieceVerified { index, bytes } => {
                    prop_assert_eq!(&bytes, &pieces[index as usize]);
                    completed.push(index);
                }
                magnetite_core::session::SessionEvent::BlockBuffered { .. } => {}
                other => prop_assert!(false, "unexpected event {other:?}"),
            }
        }
        prop_assert!(pump.is_complete());
        prop_assert!(pump.pipeline().is_empty());
        completed.sort_unstable();
        let expected: Vec<u32> = (0..piece_count).collect();
        prop_assert_eq!(completed, expected);
    }
}

proptest! {
    /// The board's ledger and the assembler's planner agree on the layout:
    /// every block the assembler plans is missing on a fresh board, chasing
    /// them all turns the endgame on, and every endgame pick is a plan the
    /// assembler would have issued for the same layout.
    #[test]
    fn endgame_board_matches_the_assembler_tiling(
        piece_count in 1_u32..=6_u32,
        piece_length in 1_u32..=40_000_u32,
        last_length in 1_u32..=40_000_u32,
    ) {
        use magnetite_core::endgame::EndgameBoard;

        let last_length = last_length.min(piece_length);
        let total =
            u64::from(piece_count - 1) * u64::from(piece_length) + u64::from(last_length);
        let mut board = EndgameBoard::new(piece_count, piece_length, total).unwrap();
        let assembler = BlockAssembler::new(piece_count, piece_length, total, 4).unwrap();

        let mut total_blocks = 0_u32;
        for index in 0..piece_count {
            for plan in assembler.block_plan(index).unwrap() {
                total_blocks += 1;
                prop_assert!(board.is_missing(plan.index, plan.begin));
                board.record_request(&plan).unwrap();
            }
        }
        prop_assert_eq!(board.missing_blocks(), total_blocks);
        prop_assert!(board.in_endgame());

        // The endgame pick walks the whole tail: every pick is an
        // assembler plan, and delivering each pick drains the ledger
        // exactly once per block.
        let mut picked = 0_u32;
        while board.missing_blocks() > 0 {
            let plan = board.pick_endgame().unwrap();
            picked += 1;
            prop_assert!(assembler.block_plan(plan.index).unwrap().contains(&plan));
            board.record_request(&plan).unwrap();
            board.mark_block(plan.index, plan.begin).unwrap();
        }
        prop_assert_eq!(picked, total_blocks);
        prop_assert!(!board.in_endgame());
        prop_assert_eq!(board.pick_endgame(), None);
    }

    /// A hostile ledger never panics: random out-of-range or duplicate
    /// operations answer with values or typed errors, the per-block chase
    /// cap holds whatever happened, and the queries stay total.
    #[test]
    fn endgame_bookkeeping_never_panics(
        piece_count in 1_u32..=6_u32,
        piece_length in 1_u32..=40_000_u32,
        last_length in 1_u32..=40_000_u32,
        operations in proptest::collection::vec((any::<u32>(), any::<u32>(), any::<u8>()), 0..64),
    ) {
        use magnetite_core::downloader::BLOCK_LEN;
        use magnetite_core::endgame::{EndgameBoard, MAX_REQUESTS_PER_BLOCK};

        let last_length = last_length.min(piece_length);
        let total =
            u64::from(piece_count - 1) * u64::from(piece_length) + u64::from(last_length);
        let mut board = EndgameBoard::new(piece_count, piece_length, total).unwrap();
        let block_bound: u32 = (0..piece_count)
            .map(|index| {
                let length = if index + 1 == piece_count {
                    last_length
                } else {
                    piece_length
                };
                length.div_ceil(BLOCK_LEN)
            })
            .sum();

        for (index, begin, op) in operations {
            let plan = BlockPlan {
                index,
                begin,
                length: BLOCK_LEN,
            };
            match op % 6 {
                0 | 1 => {
                    let _ = board.record_request(&plan);
                }
                2 => {
                    let _ = board.release_request(index, begin);
                }
                3 => {
                    let _ = board.mark_block(index, begin);
                }
                4 => {
                    let _ = board.mark_piece(index);
                }
                _ => {
                    let _ = board.reset_piece(index);
                }
            }
            // Total queries, and the cap holds no matter what happened.
            prop_assert!(board.requests_for(index, begin) <= MAX_REQUESTS_PER_BLOCK);
            let _ = board.is_missing(index, begin);
            let _ = board.is_done(index);
            prop_assert!(board.missing_blocks() <= block_bound);
            let _ = board.in_endgame();
            let _ = board.pick_endgame();
        }
    }
}
