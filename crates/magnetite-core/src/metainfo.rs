//! `.torrent` metainfo parsing (BEP 3) with raw-slice infohashing.
//!
//! The infohash is SHA-1 over the bytes of the `info` dictionary exactly as
//! they appear in the file. The decoder records that entry's byte span
//! during its single pass ([`bencode::decode_with_entry_spans`]), so hashing
//! needs no re-encode. For spec-compliant torrents — whose dictionaries are
//! already byte-sorted — the digest is identical to hashing a canonical
//! re-encoding, and for the rare non-canonical ones it is the same value
//! every mainstream client computes.

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// The `info` dictionary of a torrent: single-file or multi-file layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentInfo {
    /// A single-file torrent (`length` present).
    Single {
        /// Suggested file name.
        name: String,
        /// File size in bytes.
        length: u64,
        /// Piece size in bytes (the last piece may be shorter).
        piece_length: u64,
        /// Concatenated 20-byte SHA-1 hashes, one per piece.
        pieces: Vec<u8>,
        /// `private` flag (BEP 27).
        private: bool,
    },
    /// A multi-file torrent (`files` list present).
    Multi {
        /// Suggested directory name.
        name: String,
        /// Contained files in declaration order.
        files: Vec<TorrentFile>,
        /// Piece size in bytes.
        piece_length: u64,
        /// Concatenated 20-byte SHA-1 hashes, one per piece.
        pieces: Vec<u8>,
        /// `private` flag (BEP 27).
        private: bool,
    },
}

/// One file inside a multi-file torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    /// Size in bytes.
    pub length: u64,
    /// Path components below the torrent root directory.
    pub path: Vec<String>,
}

/// A parsed `.torrent` metainfo file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaInfo {
    /// Primary announce URL, if any.
    pub announce: Option<String>,
    /// Tiered tracker list (BEP 12).
    pub announce_list: Vec<Vec<String>>,
    /// Free-form comment.
    pub comment: Option<String>,
    /// Name of the application that created the torrent.
    pub created_by: Option<String>,
    /// Unix timestamp of creation.
    pub creation_date: Option<i64>,
    /// Parsed `info` dictionary.
    pub info: TorrentInfo,
    /// SHA-1 over the raw bytes of the `info` dictionary as they appear in
    /// the file.
    pub info_hash: InfoHash,
}

impl MetaInfo {
    /// Decodes a `.torrent` file from raw bencode bytes.
    ///
    /// # Errors
    /// Returns [`Error::Metainfo`] when required keys are missing or have the
    /// wrong shape (e.g. `pieces` length not a multiple of 20), or
    /// [`Error::Bencode`] when the input is not valid bencode at all.
    pub fn parse(input: &[u8]) -> Result<Self> {
        let (root, entry_spans) = bencode::decode_with_entry_spans(input)?;
        let Value::Dict(root) = &root else {
            return Err(Error::Metainfo(format!(
                "top-level value must be a dictionary, got {}",
                type_name(&root)
            )));
        };

        let info_value = root
            .get(&b"info"[..])
            .ok_or_else(|| Error::Metainfo("missing `info` dictionary".into()))?;
        let Value::Dict(_) = info_value else {
            return Err(Error::Metainfo(format!(
                "`info` must be a dictionary, got {}",
                type_name(info_value)
            )));
        };

        // Raw-slice infohash: SHA-1 over the bytes of the `info` dictionary
        // exactly as they appear in the file — the digest every mainstream
        // client computes. The span comes from the decoder's single pass, so
        // no canonical re-encode is needed on this path.
        let info_span = entry_spans
            .get(&b"info"[..])
            .ok_or_else(|| Error::Metainfo("missing `info` dictionary".into()))?;
        let digest: [u8; 20] = Sha1::digest(&input[info_span]).into();

        let info = parse_info(info_value)?;
        let announce = opt_string(root.get(&b"announce"[..]));
        let announce_list = parse_announce_list(root.get(&b"announce-list"[..]))?;

        Ok(Self {
            announce,
            announce_list,
            comment: opt_string(root.get(&b"comment"[..])),
            created_by: opt_string(root.get(&b"created by"[..])),
            creation_date: root.get(&b"creation date"[..]).and_then(Value::as_int),
            info,
            info_hash: InfoHash::from_bytes(digest),
        })
    }

    /// Total payload size in bytes across all files.
    #[must_use]
    pub fn total_length(&self) -> u64 {
        match &self.info {
            TorrentInfo::Single { length, .. } => *length,
            TorrentInfo::Multi { files, .. } => files.iter().map(|f| f.length).sum(),
        }
    }

    /// Number of 20-byte SHA-1 entries in `pieces`.
    #[must_use]
    pub fn piece_count(&self) -> u64 {
        let pieces = match &self.info {
            TorrentInfo::Single { pieces, .. } | TorrentInfo::Multi { pieces, .. } => pieces,
        };
        (pieces.len() / 20) as u64
    }
}

fn opt_string(field: Option<&Value<'_>>) -> Option<String> {
    match field {
        Some(Value::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

fn required_string(field: Option<&Value<'_>>, key: &str) -> Result<String> {
    match field {
        Some(Value::Bytes(b)) => Ok(String::from_utf8_lossy(b).into_owned()),
        Some(found) => Err(Error::Metainfo(format!(
            "`{key}` must be a byte string, got {}",
            type_name(found)
        ))),
        None => Err(Error::Metainfo(format!("missing `{key}`"))),
    }
}

fn required_u64(field: Option<&Value<'_>>, key: &str) -> Result<u64> {
    match field {
        Some(Value::Int(n)) => {
            u64::try_from(*n).map_err(|_| Error::Metainfo(format!("`{key}` must be non-negative")))
        }
        Some(found) => Err(Error::Metainfo(format!(
            "`{key}` must be an integer, got {}",
            type_name(found)
        ))),
        None => Err(Error::Metainfo(format!("missing `{key}`"))),
    }
}

/// The bencode type of `value`, spelled out for rejections — a vague
/// "invalid" teaches a hostile torrent nothing.
fn type_name(value: &Value<'_>) -> &'static str {
    match value {
        Value::Int(_) => "an integer",
        Value::Bytes(_) => "a byte string",
        Value::List(_) => "a list",
        Value::Dict(_) => "a dictionary",
    }
}

fn parse_info(info: &Value<'_>) -> Result<TorrentInfo> {
    let name = required_string(info.get(b"name"), "name")?;
    let piece_length = required_u64(info.get(b"piece length"), "piece length")?;
    // The taxonomy's own help text promises a *positive* piece length
    // (`magnetite::metainfo`), and every consumer derives block tiling from
    // it — zero is a contract hole, not a corner case, so it is rejected
    // here at the parse boundary instead of surfacing as a division or an
    // assembler guard downstream.
    if piece_length == 0 {
        return Err(Error::Metainfo(
            "`piece length` must be at least one byte".into(),
        ));
    }

    let pieces = match info.get(b"pieces") {
        Some(Value::Bytes(b)) => b.to_vec(),
        Some(found) => {
            return Err(Error::Metainfo(format!(
                "`pieces` must be a byte string, got {}",
                type_name(found)
            )));
        }
        None => return Err(Error::Metainfo("missing `pieces`".into())),
    };
    if pieces.len() % 20 != 0 {
        return Err(Error::Metainfo(format!(
            "`pieces` length {} is not a multiple of 20",
            pieces.len()
        )));
    }
    let private = info.get(b"private").and_then(Value::as_int) == Some(1);

    if let Some(Value::Int(length)) = info.get(b"length") {
        let length = u64::try_from(*length)
            .map_err(|_| Error::Metainfo("`length` must be non-negative".into()))?;
        return Ok(TorrentInfo::Single {
            name,
            length,
            piece_length,
            pieces,
            private,
        });
    }

    let entries = match info.get(b"files") {
        Some(Value::List(entries)) => entries,
        Some(found) => {
            return Err(Error::Metainfo(format!(
                "`files` must be a list, got {}",
                type_name(found)
            )));
        }
        None => {
            return Err(Error::Metainfo(
                "neither `length` nor `files` present".into(),
            ));
        }
    };

    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Dict(file) = entry else {
            return Err(Error::Metainfo(format!(
                "each file entry must be a dictionary, got {}",
                type_name(entry)
            )));
        };
        let length = required_u64(file.get(&b"length"[..]), "file `length`")?;
        let path_value = file
            .get(&b"path"[..])
            .ok_or_else(|| Error::Metainfo("file entry missing `path`".into()))?;
        let Value::List(parts) = path_value else {
            return Err(Error::Metainfo(format!(
                "file `path` must be a list, got {}",
                type_name(path_value)
            )));
        };
        let mut path = Vec::with_capacity(parts.len());
        for part in parts {
            let Value::Bytes(component) = part else {
                return Err(Error::Metainfo(format!(
                    "path components must be byte strings, got {}",
                    type_name(part)
                )));
            };
            path.push(String::from_utf8_lossy(component).into_owned());
        }
        files.push(TorrentFile { length, path });
    }

    Ok(TorrentInfo::Multi {
        name,
        files,
        piece_length,
        pieces,
        private,
    })
}

fn parse_announce_list(field: Option<&Value<'_>>) -> Result<Vec<Vec<String>>> {
    let Some(Value::List(tiers)) = field else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(tiers.len());
    for tier in tiers {
        let Value::List(urls) = tier else {
            return Err(Error::Metainfo(format!(
                "`announce-list` tiers must be lists, got {}",
                type_name(tier)
            )));
        };
        let mut tier_out = Vec::with_capacity(urls.len());
        for url in urls {
            let Value::Bytes(b) = url else {
                return Err(Error::Metainfo(format!(
                    "tracker URLs must be byte strings, got {}",
                    type_name(url)
                )));
            };
            tier_out.push(String::from_utf8_lossy(b).into_owned());
        }
        out.push(tier_out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    /// Canonical single-file `info` dict. The expected infohash below is
    /// computed over this raw slice directly with the sha1 crate — no
    /// round-trip through our own encoder, so encoder bugs cannot mask
    /// infohash bugs.
    const SINGLE_INFO: &[u8] = b"d6:lengthi1024e4:name8:file.bin12:piece lengthi16384e6:pieces20:\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13e";

    fn single_torrent() -> Vec<u8> {
        let mut out = b"d8:announce26:udp://tracker.example:1337".to_vec();
        out.extend_from_slice(b"13:creation datei1700000000e4:info");
        out.extend_from_slice(SINGLE_INFO);
        out.push(b'e');
        out
    }

    fn dict(pairs: Vec<(&'static [u8], Value<'static>)>) -> Value<'static> {
        Value::Dict(
            pairs
                .into_iter()
                .map(|(k, v)| (Cow::Borrowed(k), v))
                .collect(),
        )
    }

    fn bytes(b: &'static [u8]) -> Value<'static> {
        Value::Bytes(Cow::Borrowed(b))
    }

    #[test]
    fn computes_infohash_from_raw_info_slice() {
        let expected: [u8; 20] = Sha1::digest(SINGLE_INFO).into();
        let meta = MetaInfo::parse(&single_torrent()).unwrap();
        assert_eq!(meta.info_hash, InfoHash::from_bytes(expected));
    }

    #[test]
    fn parses_single_file_fields() {
        let meta = MetaInfo::parse(&single_torrent()).unwrap();
        assert_eq!(meta.announce.as_deref(), Some("udp://tracker.example:1337"));
        assert_eq!(meta.creation_date, Some(1_700_000_000));
        match &meta.info {
            TorrentInfo::Single {
                name,
                length,
                piece_length,
                pieces,
                private,
            } => {
                assert_eq!(name, "file.bin");
                assert_eq!(*length, 1024);
                assert_eq!(*piece_length, 16384);
                assert_eq!(pieces.len(), 20);
                assert!(!*private);
            }
            other @ TorrentInfo::Multi { .. } => {
                panic!("expected single-file info, got {other:?}")
            }
        }
        assert_eq!(meta.total_length(), 1024);
        assert_eq!(meta.piece_count(), 1);
    }

    #[test]
    fn parses_multi_file_torrent() {
        let piece = vec![0xAB_u8; 20];
        let info = dict(vec![
            (
                b"files",
                Value::List(vec![
                    dict(vec![
                        (b"length", Value::Int(10)),
                        (b"path", Value::List(vec![bytes(b"foo"), bytes(b"bar.b")])),
                    ]),
                    dict(vec![
                        (b"length", Value::Int(20)),
                        (b"path", Value::List(vec![bytes(b"baz")])),
                    ]),
                ]),
            ),
            (b"name", bytes(b"data")),
            (b"piece length", Value::Int(16384)),
            (b"pieces", Value::Bytes(Cow::Owned(piece))),
            (b"private", Value::Int(1)),
        ]);
        let root = dict(vec![(b"info", info)]);
        let raw = bencode::encode_to_vec(&root).unwrap();

        let meta = MetaInfo::parse(&raw).unwrap();
        match &meta.info {
            TorrentInfo::Multi {
                name,
                files,
                piece_length,
                pieces,
                private,
            } => {
                assert_eq!(name, "data");
                assert_eq!(files.len(), 2);
                assert_eq!(files[0].path, vec!["foo", "bar.b"]);
                assert_eq!(files[1].path, vec!["baz"]);
                assert_eq!(*piece_length, 16384);
                assert_eq!(pieces.len(), 20);
                assert!(*private);
            }
            other @ TorrentInfo::Single { .. } => {
                panic!("expected multi-file info, got {other:?}")
            }
        }
        assert_eq!(meta.total_length(), 30);
    }

    #[test]
    fn parses_announce_list() {
        let mut raw = single_torrent();
        // insert announce-list after announce (decoder accepts any order)
        let insert_at = b"d8:announce26:udp://tracker.example:1337".len();
        let tier = b"13:announce-listll34:udp://backup.example:6969/announceee";
        raw.splice(insert_at..insert_at, tier.iter().copied());
        let meta = MetaInfo::parse(&raw).unwrap();
        assert_eq!(
            meta.announce_list,
            vec![vec!["udp://backup.example:6969/announce"]]
        );
    }

    #[test]
    fn rejects_broken_metainfo() {
        assert!(MetaInfo::parse(b"i42e").is_err());
        assert!(MetaInfo::parse(b"de").is_err());
        assert!(MetaInfo::parse(b"d4:infoi1ee").is_err());
        assert!(MetaInfo::parse(b"").is_err());

        let bad_pieces = b"d4:infod6:lengthi1e4:name1:a12:piece lengthi16e6:pieces4:aaaee";
        assert!(MetaInfo::parse(bad_pieces).is_err());

        let no_files = b"d4:infod4:name1:a12:piece lengthi16e6:pieces20:\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13ee";
        assert!(MetaInfo::parse(no_files).is_err());
    }

    #[test]
    fn infohash_hashes_non_canonical_info_slice_as_is() {
        // `name` sorts after `length`, so this dictionary is NOT in canonical
        // byte-sorted order. Every mainstream client hashes the raw bytes
        // exactly as they appear in the file; the infohash must do the same,
        // otherwise trackers and peers see a different hash than we do.
        let info: &[u8] = b"d4:name8:file.bin6:lengthi1024e12:piece lengthi16384e6:pieces20:\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13e";
        let mut raw = b"d8:announce26:udp://tracker.example:13374:info".to_vec();
        raw.extend_from_slice(info);
        raw.push(b'e');

        let meta = MetaInfo::parse(&raw).unwrap();
        let expected: [u8; 20] = Sha1::digest(info).into();
        assert_eq!(meta.info_hash, InfoHash::from_bytes(expected));

        // Only the hashing contract changed: field extraction still works.
        match meta.info {
            TorrentInfo::Single { name, length, .. } => {
                assert_eq!(name, "file.bin");
                assert_eq!(length, 1024);
            }
            other @ TorrentInfo::Multi { .. } => {
                panic!("expected single-file info, got {other:?}")
            }
        }
    }
    #[test]
    fn rejects_a_zero_piece_length_at_the_parse_boundary() {
        // The taxonomy's help text promises a positive `piece length`; the
        // parse boundary enforces it instead of deferring to consumers.
        let info: &[u8] =
            b"d6:lengthi1024e4:name8:file.bin12:piece lengthi0e6:pieces20:\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13e";
        let mut raw = b"d8:announce26:udp://tracker.example:13374:info".to_vec();
        raw.extend_from_slice(info);
        raw.push(b'e');
        let error = MetaInfo::parse(&raw).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`piece length` must be at least one byte")
        );
    }

    #[test]
    fn rejections_say_what_is_missing_and_what_was_found() {
        // Missing keys and wrong-typed keys are different rejections; the
        // wrong-typed ones name the found bencode type. The fixtures are
        // encoded through the workspace codec so the wire shape is exact.
        let pieces = || Value::Bytes(Cow::Borrowed(&[0_u8; 20]));
        let raw = bencode::encode_to_vec(&dict(vec![(
            b"info",
            dict(vec![
                (b"length", Value::Int(1024)),
                (b"piece length", Value::Int(16_384)),
                (b"pieces", pieces()),
            ]),
        )]))
        .unwrap();
        let error = MetaInfo::parse(&raw).unwrap_err();
        assert!(error.to_string().contains("missing `name`"));

        let raw = bencode::encode_to_vec(&dict(vec![(
            b"info",
            dict(vec![
                (b"length", Value::Int(1024)),
                (b"name", Value::Int(7)),
                (b"piece length", Value::Int(16_384)),
                (b"pieces", pieces()),
            ]),
        )]))
        .unwrap();
        let error = MetaInfo::parse(&raw).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`name` must be a byte string, got an integer")
        );

        let raw = bencode::encode_to_vec(&dict(vec![(
            b"info",
            dict(vec![
                (b"length", Value::Int(1024)),
                (b"name", bytes(b"file.bin")),
                (b"piece length", Value::Int(16_384)),
                (b"pieces", Value::List(vec![])),
            ]),
        )]))
        .unwrap();
        let error = MetaInfo::parse(&raw).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`pieces` must be a byte string, got a list")
        );

        let error = MetaInfo::parse(b"i42e").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("top-level value must be a dictionary, got an integer")
        );

        let raw = bencode::encode_to_vec(&dict(vec![
            (
                b"announce-list",
                Value::List(vec![dict(vec![(b"udp", bytes(b"udp://t:80"))])]),
            ),
            (
                b"info",
                dict(vec![
                    (b"length", Value::Int(1024)),
                    (b"name", bytes(b"file.bin")),
                    (b"piece length", Value::Int(16_384)),
                    (b"pieces", pieces()),
                ]),
            ),
        ]))
        .unwrap();
        let error = MetaInfo::parse(&raw).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`announce-list` tiers must be lists, got a dictionary")
        );
    }
}
