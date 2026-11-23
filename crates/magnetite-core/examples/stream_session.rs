//! Consumer example: the `tokio` I/O edge of the session, end to end.
//!
//! This demonstrates the M1 remainder — the stream reader that turns the
//! runtime-free [`SessionPump`] into a real async connection. One program
//! spins up a loopback "tracker peer" TCP server that serves a torrent's
//! bytes, then a [`StreamSession`] connects, handshakes, and downloads the
//! whole torrent through `step` — the same end-to-end shape
//! `download_pump.rs` drives synchronously, but here `tokio` enters at the
//! socket edge and the pump stays runtime-free.
//!
//! Run it with spans visible:
//!
//! ```console
//! $ cargo run -p magnetite-core --example stream_session --features stream
//! $ RUST_LOG=debug cargo run -p magnetite-core --example stream_session --features stream
//! ```

use std::borrow::Cow;
use std::collections::BTreeMap;

use magnetite_core::bencode::{self, Value};
use magnetite_core::downloader::{PieceField, PieceStrategy};
use magnetite_core::infohash::InfoHash;
use magnetite_core::metainfo::MetaInfo;
use magnetite_core::peer_wire::{Handshake, Message};
use magnetite_core::session::SessionEvent;
use magnetite_core::session_stream::{StreamConfig, StreamSession};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Bencode-encoded BEP 3 metainfo for an 8-piece, 16 KiB-per-piece torrent:
/// small enough to run in milliseconds, large enough to exercise multi-block
/// piece assembly and the pipeline cap.
fn metainfo_bytes(content: &[u8]) -> Vec<u8> {
    let piece_length = 16_384_u32;
    let mut pieces = Vec::new();
    for chunk in content.chunks(piece_length as usize) {
        let digest: [u8; 20] = Sha1::digest(chunk).into();
        pieces.extend_from_slice(&digest);
    }
    let value = Value::Dict(BTreeMap::from([
        (
            Cow::Borrowed(b"announce".as_slice()),
            Value::Bytes(Cow::Borrowed(b"udp://tracker.example:1337/announce")),
        ),
        (
            Cow::Borrowed(b"info".as_slice()),
            Value::Dict(BTreeMap::from([
                (
                    Cow::Borrowed(b"length".as_slice()),
                    Value::Int(i64::try_from(content.len()).unwrap_or(i64::MAX)),
                ),
                (
                    Cow::Borrowed(b"name".as_slice()),
                    Value::Bytes(Cow::Borrowed(b"magnetite-stream-demo.bin")),
                ),
                (
                    Cow::Borrowed(b"piece length".as_slice()),
                    Value::Int(i64::from(piece_length)),
                ),
                (
                    Cow::Borrowed(b"pieces".as_slice()),
                    Value::Bytes(Cow::Owned(pieces)),
                ),
            ])),
        ),
    ]));
    bencode::encode_to_vec(&value).unwrap_or_default()
}

/// Deterministic payload: byte `i` is `(i * 13) % 251`.
fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 13) % 251).unwrap_or(0))
        .collect()
}

/// The "tracker peer" server: accepts one connection, handshakes, sends a
/// full bitfield, then answers every `Request` with the true bytes from
/// `payload`. A real client would talk to many peers; this demo keeps it to
/// one so the I/O edge is the whole story.
async fn serve_peer(listener: TcpListener, payload: Vec<u8>, piece_count: u32, piece_length: u32) {
    let (mut socket, _) = listener.accept().await.expect("the session connects");
    // Read the session's handshake.
    let mut buf = [0_u8; 68];
    socket.read_exact(&mut buf).await.unwrap();
    // Send ours.
    let info_hash = InfoHash::from_bytes([0xAB; 20]);
    let hs = Handshake::new(info_hash, *b"-qB4550-magnetite-y!");
    socket.write_all(&hs.encode()).await.unwrap();
    // Send a full bitfield.
    let mut wire = PieceField::new(piece_count).unwrap();
    for index in 0..piece_count {
        wire.set_have(index).unwrap();
    }
    let mut frame = Vec::new();
    Message::Bitfield(Cow::Owned(wire.to_bitfield())).encode_into(&mut frame);
    socket.write_all(&frame).await.unwrap();
    tracing::info!("peer   : handshake + bitfield sent");

    // Answer every Request until the session closes the connection. The
    // session writes a single `Piece` frame after the download completes
    // (the seeder demo), so the read loop must frame incrementally — a
    // fixed 17-byte read would land mid-frame and fail to parse.
    let mut read_buf: Vec<u8> = Vec::new();
    loop {
        // Try to parse one complete frame out of the buffer first.
        if let Some((message, consumed)) = magnetite_core::peer_wire::next_message(&read_buf)
            .expect("the peer's wire bytes are framed")
        {
            // Detach from the buffer before draining: `next_message` borrows.
            let message = message.into_owned();
            read_buf.drain(..consumed);
            let plan = match message {
                Message::Request {
                    index,
                    begin,
                    length,
                } => magnetite_core::downloader::BlockPlan {
                    index,
                    begin,
                    length,
                },
                _ => continue, // Non-Request frames are not our concern here.
            };
            let start = plan.index as usize * piece_length as usize + plan.begin as usize;
            let end = start + plan.length as usize;
            let mut frame = Vec::new();
            Message::Piece {
                index: plan.index,
                begin: plan.begin,
                block: Cow::Owned(payload[start..end].to_vec()),
            }
            .encode_into(&mut frame);
            socket.write_all(&frame).await.unwrap();
            tracing::debug!(index = plan.index, begin = plan.begin, "peer served block");
            continue;
        }
        // No complete frame: read more, or break on EOF.
        let mut chunk = [0_u8; 4096];
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break, // The session closed the connection.
            Ok(n) => read_buf.extend_from_slice(&chunk[..n]),
        }
    }
    tracing::info!("peer   : connection closed, done serving");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 1. Build a tiny torrent in memory, parse it, and take its infohash.
    let (piece_count, piece_length) = (8_u32, 16_384_u32);
    let total = u64::from(piece_count) * u64::from(piece_length);
    let content = payload(usize::try_from(total).unwrap_or(usize::MAX));
    let raw = metainfo_bytes(&content);
    let meta = MetaInfo::parse(&raw).expect("the generated metainfo is valid");
    println!(
        "torrent: {} \u{b7} {} bytes \u{d7} {} \u{d7} {} bytes",
        meta.info_hash,
        meta.total_length(),
        meta.piece_count(),
        piece_length,
    );

    // 2. The expected digests, borrowed from the metainfo's `pieces` field.
    let hashes = match &meta.info {
        magnetite_core::metainfo::TorrentInfo::Single { pieces, .. }
        | magnetite_core::metainfo::TorrentInfo::Multi { pieces, .. } => pieces.clone(),
    };

    // 3. Start the fake peer on a loopback port.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind succeeds");
    let peer_addr = listener.local_addr().unwrap();
    let server_payload = content.clone();
    let server = tokio::spawn(serve_peer(
        listener,
        server_payload,
        piece_count,
        piece_length,
    ));

    // 4. Connect, handshake, and download through the stream session.
    let stream = tokio::net::TcpStream::connect(peer_addr)
        .await
        .expect("the peer is listening");
    let (read_half, write_half) = stream.into_split();
    let info_hash = InfoHash::from_bytes([0xAB; 20]);
    let config = StreamConfig::new(&hashes, piece_count, piece_length, total)
        .with_strategy(PieceStrategy::RarestFirst);
    let mut session = StreamSession::handshake(
        read_half,
        write_half,
        info_hash,
        *b"-qB4550-magnetite-x!",
        &config,
    )
    .await
    .expect("handshake completes against the peer");
    println!("session: handshake complete against {peer_addr}");

    let mut verified = 0_u32;
    while !session.is_complete() {
        let step = session
            .step(|_, _, _| None)
            .await
            .expect("the step pumps the routed frame");
        if let magnetite_core::session_stream::StepEvent::Routed {
            event: SessionEvent::PieceVerified { index, bytes },
            ..
        } = step
        {
            verified += 1;
            assert_eq!(
                bytes,
                &content[index as usize * piece_length as usize
                    ..(index as usize + 1) * piece_length as usize]
            );
            tracing::info!(index, "piece verified");
        }
    }
    println!("done   : {verified} pieces verified through the tokio I/O edge");

    // 5. The leech becomes a seeder: serve one block out of the caller's
    // storage. (A fresh connection would ask; this demo reuses the pump.)
    let storage = content.clone();
    let block_served = session
        .serve(
            0,
            0,
            magnetite_core::downloader::BLOCK_LEN,
            move |_, begin, length| {
                let start = begin as usize;
                Some(storage[start..start + length as usize].to_vec())
            },
        )
        .await
        .expect("the served block matches the layout");
    println!("seeder : served piece 0 block 0 = {block_served}");
    // Drop the session to close the connection so the peer's read loop
    // sees EOF and exits — otherwise `server.await` blocks forever on a
    // connection the peer is still happy to serve.
    drop(session);
    server.await.unwrap();
}
