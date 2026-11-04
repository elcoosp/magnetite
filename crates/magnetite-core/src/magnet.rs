//! Magnet URI parsing and canonical re-serialization (BEP 9).
//!
//! Parsing is lenient about *which* parameters appear (unknown keys are kept
//! for round-tripping) and strict about *how* they are shaped (a malformed
//! `xt` infohash or non-numeric `xl` is an error).

use std::fmt;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// Percent-encode everything except unreserved characters — magnet values are
/// frequently URIs themselves (`tr=udp%3A%2F%2F…`), so over-encoding is both
/// correct and matches what mainstream clients emit.
const VALUE_SET: &AsciiSet = NON_ALPHANUMERIC;

/// A parsed `magnet:?` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetUri {
    /// `dn` — suggested display name.
    pub display_name: Option<String>,
    /// `xt` with a `urn:btih:` payload — the torrent infohash.
    pub info_hash: Option<InfoHash>,
    /// `xt` parameters that are not `urn:btih:` (kept for round-tripping).
    pub other_xt: Vec<String>,
    /// `tr` — tracker announce URLs (percent-decoded, deduplicated).
    pub trackers: Vec<String>,
    /// `ws` — web seed URLs (BEP 19, percent-decoded).
    pub web_seeds: Vec<String>,
    /// `x.pe` — `host:port` peer addresses (BEP 42 style, percent-decoded).
    pub peers: Vec<String>,
    /// `xl` — exact payload length in bytes.
    pub length: Option<u64>,
    /// Any other `key=value` pairs (percent-decoded, order preserved).
    pub extra: Vec<(String, String)>,
}

/// Parses a magnet URI string.
///
/// # Errors
/// Returns [`Error::Magnet`] when the URI does not start with `magnet:?`, has
/// a malformed `xt` infohash, or a non-numeric `xl`.
pub fn parse(uri: &str) -> Result<MagnetUri> {
    let rest = uri
        .strip_prefix("magnet:?")
        .ok_or_else(|| Error::Magnet("must start with `magnet:?`".into()))?;
    if rest.is_empty() {
        return Err(Error::Magnet("empty parameter list".into()));
    }

    let mut out = MagnetUri {
        display_name: None,
        info_hash: None,
        other_xt: Vec::new(),
        trackers: Vec::new(),
        web_seeds: Vec::new(),
        peers: Vec::new(),
        length: None,
        extra: Vec::new(),
    };

    for param in rest.split('&') {
        if param.is_empty() {
            continue;
        }
        let (key, value) = match param.split_once('=') {
            Some((k, v)) => (k, decode_lossy(v)),
            None => (param, String::new()),
        };
        match key {
            "xt" => handle_xt(&mut out, &value)?,
            "dn" => out.display_name = Some(value),
            "tr" => push_unique(&mut out.trackers, value),
            "ws" => push_unique(&mut out.web_seeds, value),
            "x.pe" => push_unique(&mut out.peers, value),
            "xl" => {
                out.length = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| Error::Magnet(format!("invalid `xl`: {value:?}")))?,
                );
            }
            _ => out.extra.push((key.to_owned(), value)),
        }
    }
    Ok(out)
}

fn handle_xt(out: &mut MagnetUri, value: &str) -> Result<()> {
    if let Some(hash) = value.strip_prefix("urn:btih:") {
        let parsed = InfoHash::parse(hash)?;
        if out.info_hash.is_none() {
            out.info_hash = Some(parsed);
        }
        Ok(())
    } else {
        out.other_xt.push(value.to_owned());
        Ok(())
    }
}

fn push_unique(target: &mut Vec<String>, value: String) {
    if !target.contains(&value) {
        target.push(value);
    }
}

fn decode_lossy(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

impl fmt::Display for MagnetUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut params: Vec<String> = Vec::new();
        if let Some(hash) = &self.info_hash {
            params.push(format!("xt=urn:btih:{hash}"));
        }
        for xt in &self.other_xt {
            params.push(format!("xt={}", utf8_percent_encode(xt, VALUE_SET)));
        }
        if let Some(name) = &self.display_name {
            params.push(format!("dn={}", utf8_percent_encode(name, VALUE_SET)));
        }
        for tracker in &self.trackers {
            params.push(format!("tr={}", utf8_percent_encode(tracker, VALUE_SET)));
        }
        for seed in &self.web_seeds {
            params.push(format!("ws={}", utf8_percent_encode(seed, VALUE_SET)));
        }
        for peer in &self.peers {
            params.push(format!("x.pe={}", utf8_percent_encode(peer, VALUE_SET)));
        }
        if let Some(len) = self.length {
            params.push(format!("xl={len}"));
        }
        for (key, value) in &self.extra {
            params.push(format!(
                "{}={}",
                utf8_percent_encode(key, VALUE_SET),
                utf8_percent_encode(value, VALUE_SET)
            ));
        }
        f.write_str("magnet:?")?;
        f.write_str(&params.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use data_encoding::BASE32_NOPAD;

    use super::*;

    const HEX: &str = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";

    #[test]
    fn parses_typical_uri() {
        let uri = "magnet:?xt=urn:btih:C12FE1C06BBA254A9DC9F519B335AA7C1367A88A&dn=file.iso&tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce";
        let m = parse(uri).unwrap();
        assert_eq!(m.info_hash.unwrap().to_hex(), HEX);
        assert_eq!(m.display_name.as_deref(), Some("file.iso"));
        assert_eq!(m.trackers, vec!["udp://tracker.example:1337/announce"]);
    }

    #[test]
    fn parses_base32_infohash() {
        let b32 = BASE32_NOPAD.encode(&[0u8; 20]);
        let uri = format!("magnet:?xt=urn:btih:{b32}");
        let m = parse(&uri).unwrap();
        assert_eq!(m.info_hash.unwrap().as_bytes(), [0u8; 20]);
    }

    #[test]
    fn collects_extras_and_dedups_trackers() {
        let uri = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&tr=udp%3A%2F%2Fa&tr=udp%3A%2F%2Fa&x.pe=1.2.3.4%3A6881&ws=http%3A%2F%2Fseed&xl=1024&so=0,2-4&xt=urn:sha1:ABC";
        let m = parse(uri).unwrap();
        assert_eq!(m.trackers, vec!["udp://a"]);
        assert_eq!(m.peers, vec!["1.2.3.4:6881"]);
        assert_eq!(m.web_seeds, vec!["http://seed"]);
        assert_eq!(m.length, Some(1024));
        assert_eq!(m.extra, vec![("so".to_string(), "0,2-4".to_string())]);
        assert_eq!(m.other_xt, vec!["urn:sha1:ABC"]);
    }

    #[test]
    fn first_btih_wins() {
        let second = format!("{:040x}", 0xAu32);
        let uri = format!("magnet:?xt=urn:btih:{HEX}&xt=urn:btih:{second}");
        let m = parse(&uri).unwrap();
        assert_eq!(m.info_hash.unwrap().to_hex(), HEX);
    }

    #[test]
    fn rejects_non_magnet_and_bad_fields() {
        assert!(parse("http://example.com").is_err());
        assert!(parse("magnet:?").is_err());
        assert!(parse("magnet:?xt=urn:btih:zzzz").is_err());
        assert!(parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88").is_err());
        assert!(parse("magnet:?xl=abc").is_err());
    }

    #[test]
    fn display_round_trips() {
        let uri = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=a%20b.mkv&tr=udp%3A%2F%2Ft%3A80";
        let m = parse(uri).unwrap();
        let re = m.to_string();
        let m2 = parse(&re).unwrap();
        assert_eq!(m, m2);
        assert_eq!(
            re,
            "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=a%20b%2Emkv&tr=udp%3A%2F%2Ft%3A80"
        );
    }
}
