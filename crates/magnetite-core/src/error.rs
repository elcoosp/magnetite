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
