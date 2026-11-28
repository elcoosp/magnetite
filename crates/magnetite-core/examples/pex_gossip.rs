//! A consumer demo of the PEX seam: two session pumps over one torrent
//! exchange peers through BEP 10 / BEP 11, runtime-free — the extended
//! handshake arms `ut_pex`, the first exchange carries the full roster,
//! and the next one announces exactly one add and one drop. Run it with:
//!
//! ```console
//! $ cargo run -p magnetite-core --example pex_gossip
//! ```

use std::borrow::Cow;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use magnetite_core::downloader::PieceStrategy;
use magnetite_core::peer_wire::{EXT_HANDSHAKE_ID, Message};
use magnetite_core::pex::{ExtHandshake, PexFlags, PexLedger, PexPeer};
use magnetite_core::session::{SessionEvent, SessionPump};

/// Two fake digest tables: the demo only exercises PEX routing, so the
/// digests never face the verifier.
fn digests() -> Vec<u8> {
    let mut digests = Vec::new();
    for piece in 0..2_u8 {
        digests.extend(std::iter::repeat_n(piece + 1, 20));
    }
    digests
}

/// One peer on the demo swarm with its flag byte.
fn peer(last_octet: u8, flags: u8) -> PexPeer {
    PexPeer {
        addr: SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(192, 0, 2, last_octet),
            6_881,
        )),
        flags: PexFlags(flags),
    }
}

/// Wraps a bencoded payload in an extended frame carrying `ext_id`.
fn extended(ext_id: u8, payload: &[u8]) -> Message<'static> {
    Message::Extended {
        ext_id,
        payload: Cow::Owned(payload.to_vec()),
    }
}

fn main() -> Result<(), magnetite_core::Error> {
    let digests = digests();
    let mut alice = SessionPump::new(
        &digests,
        2,
        16,
        32,
        8,
        magnetite_core::session::MAX_PIPELINE_DEPTH,
        PieceStrategy::Sequential,
    )?;
    let mut bob = SessionPump::new(
        &digests,
        2,
        16,
        32,
        8,
        magnetite_core::session::MAX_PIPELINE_DEPTH,
        PieceStrategy::Sequential,
    )?;

    // Both ends advertise PEX in their extended handshakes (BEP 10): the
    // negotiated ut_pex id is local to each direction.
    let alice_handshake = ExtHandshake {
        client: Some("magnetite-pex-gossip/0.7.0".into()),
        listen_port: Some(6_881),
        extensions: [("ut_pex".to_string(), 2_u8)].into_iter().collect(),
        max_requests: Some(250),
        ..ExtHandshake::default()
    };
    let bob_handshake = ExtHandshake::advertising_pex(7);

    match alice.handle_message(&bob_wire(&bob_handshake))? {
        SessionEvent::ExtendedHandshake { client, ut_pex_id } => {
            println!("alice <- bob extended handshake: client {client:?}, ut_pex id {ut_pex_id:?}");
        }
        other => unreachable!("unexpected first event: {other:?}"),
    }
    match bob.handle_message(&alice_wire(&alice_handshake))? {
        SessionEvent::ExtendedHandshake { client, ut_pex_id } => {
            println!(
                "bob   <- alice extended handshake: client {client:?}, ut_pex id {ut_pex_id:?}"
            );
        }
        other => unreachable!("unexpected first event: {other:?}"),
    }

    // Alice's initial PEX message carries the full roster — three peers,
    // one of them a seed that also speaks uTP.
    let mut alice_ledger = PexLedger::new();
    let roster = [
        peer(1, PexFlags::SUPPORTS_UTP),
        peer(2, PexFlags::SEED),
        peer(3, 0),
    ];
    let initial = alice_ledger.delta(roster.iter().copied());
    let bob_event = bob.handle_message(&extended(2, &initial.encode()))?;
    println!("bob   <- alice initial exchange: {bob_event:?}");

    // Bob answers with his own (single-peer) roster.
    let mut bob_ledger = PexLedger::new();
    let bob_roster = [peer(9, PexFlags::PREFERS_ENCRYPTION)];
    let answer = bob_ledger.delta(bob_roster.iter().copied());
    let alice_event = alice.handle_message(&extended(7, &answer.encode()))?;
    println!("alice <- bob initial exchange: {alice_event:?}");

    // The swarm moves: peer 2 disconnects, peer 4 connects. The delta
    // announces exactly the change — and never the same address as both
    // added and dropped (BEP 11's shape rule the codec enforces).
    let moved_roster = [peer(1, PexFlags::SUPPORTS_UTP), peer(3, 0), peer(4, 0)];
    let delta = alice_ledger.delta(moved_roster.iter().copied());
    let bob_event = bob.handle_message(&extended(2, &delta.encode()))?;
    println!("bob   <- alice delta exchange: {bob_event:?}");
    assert_eq!(delta.dropped, vec![peer(2, PexFlags::SEED).addr]);
    assert_eq!(
        bob_event,
        SessionEvent::PeersDiscovered {
            added: vec![peer(4, 0)],
            dropped: vec![peer(2, PexFlags::SEED).addr],
        }
    );

    println!(
        "final views: alice knows {} peers, bob knows {}",
        alice_ledger.known_count(),
        bob_ledger.known_count()
    );
    Ok(())
}

/// Alice's handshake rides an extended frame toward bob.
fn alice_wire(handshake: &ExtHandshake) -> Message<'static> {
    extended(EXT_HANDSHAKE_ID, &handshake.encode())
}

/// Bob's handshake likewise; the extended id the receiver sees is the
/// sender's own negotiation.
fn bob_wire(handshake: &ExtHandshake) -> Message<'static> {
    extended(EXT_HANDSHAKE_ID, &handshake.encode())
}
