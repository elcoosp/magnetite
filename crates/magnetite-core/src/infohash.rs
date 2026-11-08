//! Torrent infohash: the 20-byte SHA-1 digest of the `info` dictionary.
//!
//! On the wire it appears in two spellings — 40 hex digits (BEP 3) or 32
//! unpadded base32 digits (BEP 9) — and both are handled here.

use std::fmt;

use data_encoding::BASE32_NOPAD;

use crate::error::{Error, Result};

/// A 20-byte `BitTorrent` infohash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InfoHash([u8; 20]);

impl InfoHash {
    /// Creates an infohash from raw SHA-1 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Returns the underlying 20 bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 20] {
        self.0
    }

    /// Parses a 40-character hex infohash (case-insensitive).
    ///
    /// # Errors
    /// Returns [`Error::InfoHash`] unless the string is exactly 40 hex digits.
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).map_err(|e| Error::InfoHash(format!("bad hex: {e}")))?;
        Self::try_from_bytes(&bytes)
    }

    /// Parses a 32-character unpadded base32 infohash (case-insensitive).
    ///
    /// # Errors
    /// Returns [`Error::InfoHash`] unless the string is exactly 32 base32 digits.
    pub fn from_base32(s: &str) -> Result<Self> {
        let upper = s.to_ascii_uppercase();
        let bytes = BASE32_NOPAD
            .decode(upper.as_bytes())
            .map_err(|e| Error::InfoHash(format!("bad base32: {e}")))?;
        Self::try_from_bytes(&bytes)
    }

    /// Parses hex first, then base32 — the two on-the-wire forms of BEP 9.
    ///
    /// # Errors
    /// Returns [`Error::InfoHash`] when neither form matches.
    pub fn parse(s: &str) -> Result<Self> {
        Self::from_hex(s).or_else(|_| Self::from_base32(s))
    }

    /// Lowercase hex encoding (the canonical display form).
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    fn try_from_bytes(bytes: &[u8]) -> Result<Self> {
        let arr: [u8; 20] = bytes
            .try_into()
            .map_err(|_| Error::InfoHash(format!("expected 20 bytes, got {}", bytes.len())))?;
        Ok(Self(arr))
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHash({self})")
    }
}

#[cfg(test)]
mod tests {
    use data_encoding::BASE32_NOPAD;

    use super::*;

    #[test]
    fn parses_hex_case_insensitively() {
        let h = InfoHash::from_hex("C12FE1C06BBA254A9DC9F519B335AA7C1367A88A").unwrap();
        assert_eq!(h.to_hex(), "c12fe1c06bba254a9dc9f519b335aa7c1367a88a");
        assert_eq!(
            h,
            InfoHash::parse("c12fe1c06bba254a9dc9f519b335aa7c1367a88a").unwrap()
        );
    }

    #[test]
    fn parses_base32() {
        let expected = InfoHash::from_bytes([0xAB; 20]);
        let b32 = BASE32_NOPAD.encode(&[0xAB; 20]);
        assert_eq!(InfoHash::from_base32(&b32).unwrap(), expected);
        assert_eq!(
            InfoHash::from_base32(&b32.to_lowercase()).unwrap(),
            expected
        );
    }

    #[test]
    fn parse_falls_back_to_base32() {
        let b32 = BASE32_NOPAD.encode(&[0x01; 20]);
        assert_eq!(
            InfoHash::parse(&b32).unwrap(),
            InfoHash::from_bytes([0x01; 20])
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(InfoHash::from_hex("nothex").is_err());
        assert!(InfoHash::from_hex("c12f").is_err());
        assert!(InfoHash::from_base32("AAAA").is_err());
        assert!(InfoHash::from_base32("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1").is_err());
    }
}
