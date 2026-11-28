//! Consumer demo: a two-node BEP 5 introduction over loopback UDP.
//!
//! Run it with `cargo run -p magnetite-dht --example dht_bootstrap`. Two
//! [`DhtNode`]s bind ephemeral loopback ports; node `beta` pings node
//! `alpha` (both routing tables record the counterpart), asks `alpha` for
//! the contacts nearest a target id, pulls an announce token out of
//! `get_peers`, announces a fake swarm, and a second `get_peers` returns
//! the announced endpoint in `values` — the full introduction flow the
//! unit specs pin, printed step by step.

use std::thread;
use std::time::Duration;

use magnetite_dht::node::DhtNode;
use magnetite_dht::node_id::NodeId;
use magnetite_dht::table::KBUCKET_SIZE;

fn main() -> Result<(), magnetite_core::Error> {
    let timeout = Duration::from_secs(5);
    let alpha = DhtNode::bind(NodeId::from_bytes([0xAA; 20]), timeout)?;
    let beta = DhtNode::bind(NodeId::from_bytes([0xBB; 20]), timeout)?;
    let alpha_addr = alpha.local_addr()?;
    let beta_addr = beta.local_addr()?;
    let alpha_id = alpha.self_id();
    let beta_id = beta.self_id();
    // A swarm only these two nodes know about; the info hash is arbitrary.
    let info_hash = [0x42_u8; 20];
    println!("alpha {} at {alpha_addr}", alpha.self_id());
    println!("beta  {beta_id} at {beta_addr}");

    // The server side runs on its own thread because both halves share one
    // blocking socket; `serve_once` answers exactly one datagram per call,
    // so the demo counts out the exchanges it needs.
    let server = thread::spawn(move || {
        let mut alpha = alpha;
        let mut events = Vec::new();
        // beta asks five times: ping, find_node, get_peers, announce_peer,
        // and the second get_peers.
        for _ in 0..5 {
            match alpha.serve_once() {
                Ok(event) => events.push(event),
                Err(err) => return Err(err),
            }
        }
        Ok((alpha, events))
    });

    // 1. ping: the pong carries alpha's id, and alpha's table records beta.
    let pong = beta.ping(alpha_addr)?;
    println!("\n[1] ping  -> pong from {pong}");
    assert_eq!(pong, alpha_id, "the pong carries the replier's id");

    // 2. find_node: alpha answers with its closest contacts for beta's id —
    //    beta itself, straight out of the table the ping just filled.
    let contacts = beta.find_node(alpha_addr, &beta_id)?;
    println!("[2] find_node(beta) -> {} contact(s) back", contacts.len());

    // 3. get_peers: alpha hands out a rotating announce token (and closer
    //    contacts when the swarm is empty).
    let reply = beta.get_peers(alpha_addr, &info_hash)?;
    println!(
        "[3] get_peers -> token {} bytes, {} value(s), {} node(s)",
        reply.token.len(),
        reply.values.len(),
        reply.nodes.len()
    );

    // 4. announce_peer: beta claims the swarm on port 6_881, echoing the
    //    token; a bad token would come back as KRPC error 203.
    beta.announce_peer(alpha_addr, &info_hash, 6_881, &reply.token)?;
    println!("[4] announce_peer(port 6881) accepted");

    // 5. get_peers again: the stored endpoint comes back in `values` —
    //    exactly what a real swarm lookup hands a BitTorrent client.
    let reply = beta.get_peers(alpha_addr, &info_hash)?;
    println!("[5] get_peers -> values: {:?}", reply.values);
    println!(
        "    (beta's table holds {} contact(s); the K-bucket cap is {KBUCKET_SIZE})",
        beta.table().len(),
    );

    let (alpha, events) = server
        .join()
        .expect("the server thread answers every datagram")?;
    println!("\nalpha served {events:?}");
    println!(
        "alpha's store now holds {} swarm(s) with {} announced endpoint(s); its table holds {} contact(s)",
        alpha.store().len(),
        alpha.store().peers(&info_hash).len(),
        alpha.table().len(),
    );
    Ok(())
}
