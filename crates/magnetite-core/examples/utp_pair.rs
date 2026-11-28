//! A consumer demo of the uTP seam: an initiator and a listener complete
//! a BEP 29 handshake, exchange data with one deliberately out-of-order
//! delivery that buffers and then fills, and close with FIN — all
//! runtime-free, over in-memory bytes. Run it with:
//!
//! ```console
//! $ cargo run -p magnetite-core --example utp_pair
//! ```

use magnetite_core::utp::{Connection, ConnectionState};

fn main() {
    // The initiator picks a random connection id; the listener answers on
    // the next id up, mirroring BEP 29's two-direction pairing.
    let mut initiator = Connection::connect(0xBEEF);
    let syn = initiator.syn_packet(1_000);
    println!(
        "initiator -> ST_SYN   seq {} on connection {}",
        syn.header.seq_nr, syn.header.connection_id
    );

    let mut listener = Connection::accept_syn(&syn.header);
    let answer = listener.state_packet(1_100);
    println!(
        "listener  -> ST_STATE ack {} on connection {}",
        answer.header.ack_nr, answer.header.connection_id
    );

    let events = initiator
        .receive(&answer.encode(), 1_200)
        .expect("the handshake completes");
    println!("initiator <- handshake ack: {events:?}");
    assert_eq!(initiator.state(), ConnectionState::Connected);

    // Two data packets. Deliver the second one first on purpose: it
    // buffers, then the first fills the gap and both deliver in order.
    let payload = [0xAB_u8; 2_048];
    let packets = initiator.send(&payload, 1_300);
    let (second, first) = (&packets[1], &packets[0]);
    let events = listener
        .receive(&second.encode(), 1_400)
        .expect("legal data");
    println!(
        "listener  <- future seq: buffered {} bytes",
        summarize(&events)
    );
    let events = listener
        .receive(&first.encode(), 1_500)
        .expect("legal data");
    println!("listener  <- gap fill:    {} delivered", summarize(&events));

    let ack = listener.state_packet(1_600);
    let events = initiator.receive(&ack.encode(), 1_700).expect("legal ack");
    println!("initiator <- {events:?}");

    // FIN: the initiator closes its side; the listener answers with the
    // ack that retires the FIN, and both ends are closed.
    let fin = initiator.fin_packet(1_800);
    let events = listener.receive(&fin.encode(), 1_900).expect("legal fin");
    println!("listener  <- peer closed: {events:?}");
    let ack = listener.state_packet(2_000);
    let events = initiator
        .receive(&ack.encode(), 2_100)
        .expect("the fin ack retires");
    println!("initiator <- fin acked:   {events:?}");
    println!(
        "final states: initiator {:?}, listener {:?}",
        initiator.state(),
        listener.state()
    );
}

/// A one-line summary of the events a receive call produced, without
/// dumping megabytes of payload.
fn summarize(events: &[magnetite_core::utp::Incoming]) -> String {
    events
        .iter()
        .map(|event| match event {
            magnetite_core::utp::Incoming::Data { seq_nr, payload } => {
                format!("seq {seq_nr} ({} bytes)", payload.len())
            }
            magnetite_core::utp::Incoming::Buffered { seq_nr, len } => {
                format!("seq {seq_nr} ({len} bytes waiting)")
            }
            magnetite_core::utp::Incoming::Ack { acked } => format!("acked {}", acked.len()),
            magnetite_core::utp::Incoming::Fin { seq_nr } => format!("fin seq {seq_nr}"),
            magnetite_core::utp::Incoming::Reset => "reset".to_string(),
            magnetite_core::utp::Incoming::Stale => "stale".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
