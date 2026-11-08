//! Unified, typed error taxonomy for all magnetite-core operations.

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
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A bencode decoding failure, with the byte offset where it occurred.
    #[error("invalid bencode at byte {offset}: {kind}")]
    Bencode {
        /// Byte offset into the input where the failure was detected.
        offset: usize,
        /// The specific grammar violation.
        kind: BencodeErrorKind,
    },
    /// A syntactically fine value violated metainfo semantics.
    #[error("invalid metainfo: {0}")]
    Metainfo(String),
    /// An infohash string was neither valid hex nor valid base32.
    #[error("invalid infohash: {0}")]
    InfoHash(String),
    /// A magnet URI could not be parsed.
    #[error("invalid magnet URI: {0}")]
    Magnet(String),
    /// A tracker exchange failed: malformed reply, mismatched transaction
    /// id, timeout, or an action-3 error message from the tracker.
    #[error("tracker error: {0}")]
    Tracker(String),
    /// Underlying I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}
