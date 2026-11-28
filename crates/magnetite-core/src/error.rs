//! Unified, typed error taxonomy for all magnetite-core operations.
//!
//! Every variant is a `thiserror::Error` and a `miette::Diagnostic`: the
//! `Display` text stays terse, while stable diagnostic codes and actionable
//! help text let embedders (and the CLI) render full reports without
//! parsing prose.

use std::io;

/// Convenient result alias used throughout magnetite-core.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Fine-grained reason for a bencode parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BencodeErrorKind {
    /// Input ended in the middle of a value.
    #[error("unexpected end of input")]
    Truncated,
    /// A byte appeared where the grammar requires something else.
    #[error("unexpected byte {0:#04x}")]
    UnexpectedByte(u8),
    /// An integer payload is malformed or overflows `i64`.
    #[error("integer payload is malformed or out of range")]
    InvalidInteger,
    /// A dictionary key must be a byte string.
    #[error("dictionary key must be a byte string")]
    InvalidDictKey,
    /// Extra bytes remain after the top-level value.
    #[error("trailing bytes after top-level value")]
    TrailingBytes,
    /// Nesting exceeded [`crate::bencode::MAX_DEPTH`].
    #[error("nesting depth limit exceeded")]
    DepthLimit,
}

/// Unified error type for all magnetite-core fallible operations.
///
/// Every variant carries a stable `miette` diagnostic code and actionable
/// help text, so embedders can render `miette` reports (or surface the
/// `help` strings in their own UIs) without parsing prose.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[diagnostic()]
pub enum Error {
    /// A bencode decoding failure, with the byte offset where it occurred.
    #[error("invalid bencode at byte {offset}: {kind}")]
    #[diagnostic(
        code(magnetite::bencode),
        help(
            "bencode grammar is `i<int>e`, `<len>:<bytes>`, `l<value>*e` and \
             `d<key:value>*e`; the offset points at the offending byte"
        )
    )]
    Bencode {
        /// Byte offset into the input where the failure was detected.
        offset: usize,
        /// The specific grammar violation.
        kind: BencodeErrorKind,
    },
    /// A syntactically fine value violated metainfo semantics.
    #[error("invalid metainfo: {0}")]
    #[diagnostic(
        code(magnetite::metainfo),
        help(
            "check the torrent against BEP 3: the `info` dict needs a \
             string `name`, a positive `piece length`, `pieces` in multiples \
             of 20 bytes, and exactly one of `length` or `files`"
        )
    )]
    Metainfo(String),
    /// An infohash string was neither valid hex nor valid base32.
    #[error("invalid infohash: {0}")]
    #[diagnostic(
        code(magnetite::infohash),
        help(
            "an infohash is 40 hex digits (BEP 3) or 32 unpadded base32 \
             digits (BEP 9), case-insensitive"
        )
    )]
    InfoHash(String),
    /// A magnet URI could not be parsed.
    #[error("invalid magnet URI: {0}")]
    #[diagnostic(
        code(magnetite::magnet),
        help(
            "a magnet URI starts `magnet:?xt=urn:btih:<40-hex or 32-base32>`; \
             see BEP 9 for the full parameter list"
        )
    )]
    Magnet(String),
    /// A tracker exchange failed: malformed reply, mismatched transaction
    /// id, timeout, or an action-3 error message from the tracker.
    #[error("tracker error: {0}")]
    #[diagnostic(
        code(magnetite::tracker),
        help(
            "tracker URLs are `udp://host:port[/path]`; replies must echo the \
             request's transaction id, carry action 0-3, and match the fixed \
             wire layouts of BEP 15"
        )
    )]
    Tracker(String),
    /// A peer wire exchange failed: malformed handshake, malformed message
    /// framing, or a length prefix beyond the safety cap.
    #[error("peer error: {0}")]
    #[diagnostic(
        code(magnetite::peer_wire),
        help(
            "handshakes begin with 19, the protocol string, 8 reserved bytes \
             and two 20-byte ids; message frames are a 4-byte big-endian \
             length, one id byte, and the payload (BEP 3/BEP 10)"
        )
    )]
    Peer(String),
    /// A piece-download bookkeeping failure: malformed bitfield, piece or
    /// block out of range, duplicate or misaligned block, inconsistent
    /// piece layout, or an assembler guard tripped.
    #[error("download error: {0}")]
    #[diagnostic(
        code(magnetite::download),
        help(
            "piece indexes are 0-based and bounded by the torrent layout; \
             blocks are 16 KiB-aligned, unique, and must match the planned \
             length (see BlockAssembler and RequestPipeline)"
        )
    )]
    Download(String),
    /// A DHT exchange failed (BEP 5): a malformed KRPC datagram, a
    /// mismatched transaction id, a KRPC error reply, a routing-table
    /// guard, or a UDP timeout.
    #[error("dht error: {0}")]
    #[diagnostic(
        code(magnetite::dht),
        help(
            "KRPC datagrams are bencoded dicts with a byte-string `t` \
             transaction id, a `y` type of `q`, `r` or `e`, and — for \
             queries — a `q` method name with an `a` arguments dict \
             (see BEP 5); ids, targets and info hashes are 20 bytes"
        )
    )]
    Dht(String),
    /// A peer-exchange frame violated BEP 10/BEP 11: a malformed extended
    /// handshake or `ut_pex` dict, a compact-peer string whose length is
    /// not a multiple of the 6/18-byte contact size, a `.f` flag count
    /// that disagrees with its peer count, or an address both added and
    /// dropped in one message.
    #[error("peer exchange error: {0}")]
    #[diagnostic(
        code(magnetite::pex),
        help(
            "the BEP 10 extended handshake is a bencoded dict whose `m` \
             table maps extension names to message ids; a `ut_pex` dict \
             (BEP 11) carries `added`/`added6`/`dropped`/`dropped6` as \
             6-byte IPv4 and 18-byte IPv6 compact contacts, with optional \
             `added.f`/`added6.f` flag strings one byte per peer"
        )
    )]
    Pex(String),
    /// A uTP (BEP 29) wire or bookkeeping failure: a header shorter than
    /// 20 bytes, an unknown packet type or protocol version, an extension
    /// run that overruns the packet, or a connection-side guard.
    #[error("utp error: {0}")]
    #[diagnostic(
        code(magnetite::utp),
        help(
            "a uTP packet is a fixed 20-byte header — type+version, \
             extension, connection id, two µs timestamps, window, seq and \
             ack numbers, all big-endian — optionally followed by an \
             extension chain and payload (see BEP 29); the type lives in \
             the high nibble of byte 0 and the version in the low nibble"
        )
    )]
    Utp(String),
    /// Underlying I/O failure.
    #[error("I/O error: {0}")]
    #[diagnostic(code(magnetite::io))]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The taxonomy carries actionable help text and stable diagnostic
    /// codes, so embedders can render `miette` reports without parsing
    /// prose. The `Io` variant is the one exception: the wrapped
    /// [`io::Error`] already says what failed (path, permission, ...), so
    /// it carries only the code.
    #[test]
    fn diagnostics_carry_help_and_codes() {
        let cases: Vec<Error> = vec![
            Error::Bencode {
                offset: 7,
                kind: BencodeErrorKind::TrailingBytes,
            },
            Error::Metainfo("pieces is not a multiple of 20".into()),
            Error::InfoHash("zzzz".into()),
            Error::Magnet("no xt parameter".into()),
            Error::Tracker("transaction id mismatch".into()),
            Error::Peer("handshake too short".into()),
            Error::Download("piece 1 block at 16 is already received".into()),
            Error::Dht("KRPC transaction id mismatch".into()),
            Error::Pex("`added.f` disagrees with the added peer count".into()),
            Error::Utp("header is 18 bytes, shorter than the 20-byte minimum".into()),
        ];
        for error in &cases {
            let help = miette::Diagnostic::help(error);
            assert!(help.is_some(), "missing help for {error}");
            let code = miette::Diagnostic::code(error);
            let code = code.expect("missing code").to_string();
            assert!(
                code.starts_with("magnetite::"),
                "code {code} is not namespaced"
            );
        }
        // Io: code only — the cause chain already explains the failure.
        let error = Error::Io(io::Error::new(io::ErrorKind::NotFound, "no such file"));
        let code = miette::Diagnostic::code(&error)
            .expect("missing code")
            .to_string();
        assert_eq!(code, "magnetite::io");
    }
}
