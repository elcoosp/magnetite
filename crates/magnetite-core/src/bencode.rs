//! Single-pass, allocation-conscious bencode codec.
//!
//! The decoder borrows byte strings directly from the input buffer ([`Cow`]),
//! so parsing a `.torrent` performs no per-byte allocation. Dictionary keys
//! are normalized into a [`BTreeMap`], which makes re-encoding canonical per
//! the `BitTorrent` spec (keys sorted byte-wise) and gives O(log n) lookups.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::ops::Range;

use crate::error::{BencodeErrorKind, Error, Result};

/// Hard cap on nesting depth; guards against stack exhaustion on hostile input.
pub const MAX_DEPTH: u32 = 512;

/// A bencode value. Byte strings are [`Cow`]s: borrowed when decoded, owned
/// when constructed by callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    /// `i<num>e` — signed 64-bit integer.
    Int(i64),
    /// `<len>:<bytes>` — raw byte string.
    Bytes(Cow<'a, [u8]>),
    /// `l<values>e` — ordered sequence.
    List(Vec<Value<'a>>),
    /// `d<key value pairs>e` — dictionary with byte-wise sorted keys.
    Dict(BTreeMap<Cow<'a, [u8]>, Value<'a>>),
}

impl Value<'_> {
    /// Returns the integer if this is a [`Value::Int`].
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the byte slice if this is a [`Value::Bytes`].
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the list slice if this is a [`Value::List`].
    #[must_use]
    pub fn as_list(&self) -> Option<&[Value<'_>]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    /// Returns the dictionary if this is a [`Value::Dict`].
    #[must_use]
    pub fn as_dict(&self) -> Option<&BTreeMap<Cow<'_, [u8]>, Value<'_>>> {
        match self {
            Value::Dict(map) => Some(map),
            _ => None,
        }
    }

    /// Dictionary lookup convenience: returns the value stored under `key`.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&Value<'_>> {
        match self {
            Value::Dict(map) => map.get(key),
            _ => None,
        }
    }
}

/// Decodes exactly one bencode value covering the whole input.
///
/// # Errors
/// Returns [`Error::Bencode`] with the offending byte offset on any grammar
/// violation, trailing bytes, or when nesting exceeds [`MAX_DEPTH`].
pub fn decode(input: &[u8]) -> Result<Value<'_>> {
    parse_top(input, false).map(|(value, _)| value)
}

/// Byte spans of the values stored directly under the keys of the top-level
/// dictionary, as recorded by [`decode_with_entry_spans`].
#[derive(Debug, Clone)]
pub struct EntrySpans<'a> {
    entries: Vec<(Cow<'a, [u8]>, Range<usize>)>,
}

impl EntrySpans<'_> {
    /// Byte span of the encoded value stored under `key`.
    ///
    /// A duplicated top-level key resolves to its last occurrence, mirroring
    /// how the decoded dictionary itself is built. Returns `None` when the
    /// key is absent or the top-level value was not a dictionary.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<Range<usize>> {
        self.entries
            .iter()
            .rev()
            .find(|(candidate, _)| candidate.as_ref() == key)
            .map(|(_, span)| span.clone())
    }

    /// Iterates the recorded `(key, span)` pairs; one entry per distinct key.
    pub fn iter(&self) -> impl Iterator<Item = (&'_ [u8], Range<usize>)> {
        self.entries
            .iter()
            .map(|(key, span)| (&**key, span.clone()))
    }

    /// `true` when no top-level entries were recorded (the top-level value
    /// was not a dictionary, or it was empty).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Decodes exactly one bencode value like [`decode`], and additionally
/// records the byte span of the encoded value stored under each key of the
/// top-level dictionary.
///
/// Spans address the *raw* input: `&input[spans.get(key)]` is the exact byte
/// range the value was decoded from, which is what raw-slice consumers such
/// as infohashing need. Recording costs one span push per top-level entry;
/// nested values are not tracked.
///
/// # Errors
/// Same as [`decode`].
pub fn decode_with_entry_spans(input: &[u8]) -> Result<(Value<'_>, EntrySpans<'_>)> {
    parse_top(input, true)
}

fn parse_top(input: &[u8], track_entry_spans: bool) -> Result<(Value<'_>, EntrySpans<'_>)> {
    let mut parser = Parser {
        input,
        pos: 0,
        track_entry_spans,
        entry_spans: Vec::new(),
    };
    let value = parser.value(0)?;
    if parser.pos != input.len() {
        return Err(Error::Bencode {
            offset: parser.pos,
            kind: BencodeErrorKind::TrailingBytes,
        });
    }
    Ok((
        value,
        EntrySpans {
            entries: parser.entry_spans,
        },
    ))
}

/// Appends the canonical bencode encoding of `value` to `out`.
///
/// # Errors
/// Returns [`Error::Bencode`] with [`BencodeErrorKind::DepthLimit`] when the
/// value nests deeper than [`MAX_DEPTH`] (the encoder would otherwise recurse
/// without bound).
pub fn encode(value: &Value<'_>, out: &mut Vec<u8>) -> Result<()> {
    write_value(value, out, 0)
}

/// Encodes `value` into a fresh vector.
///
/// # Errors
/// Same as [`encode`].
pub fn encode_to_vec(value: &Value<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode(value, &mut out)?;
    Ok(out)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    /// Only [`decode_with_entry_spans`] pays for span recording.
    track_entry_spans: bool,
    entry_spans: Vec<(Cow<'a, [u8]>, Range<usize>)>,
}

impl<'a> Parser<'a> {
    fn error(&self, kind: BencodeErrorKind) -> Error {
        Error::Bencode {
            offset: self.pos,
            kind,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn value(&mut self, depth: u32) -> Result<Value<'a>> {
        Ok(self.value_with_span(depth)?.0)
    }

    /// Parses one value and reports the byte span it was encoded at.
    fn value_with_span(&mut self, depth: u32) -> Result<(Value<'a>, Range<usize>)> {
        let start = self.pos;
        if depth > MAX_DEPTH {
            return Err(self.error(BencodeErrorKind::DepthLimit));
        }
        let value = match self.peek() {
            Some(b'i') => self.integer()?,
            Some(b'0'..=b'9') => Value::Bytes(self.byte_string()?),
            Some(b'l') => self.list(depth)?,
            Some(b'd') => self.dict(depth)?,
            Some(other) => return Err(self.error(BencodeErrorKind::UnexpectedByte(other))),
            None => return Err(self.error(BencodeErrorKind::Truncated)),
        };
        Ok((value, start..self.pos))
    }

    fn integer(&mut self) -> Result<Value<'a>> {
        self.pos += 1; // consume 'i'
        let start = self.pos;
        let end = loop {
            match self.bump() {
                Some(b'e') => break self.pos - 1,
                Some(b) if b.is_ascii_digit() || (b == b'-' && self.pos - 1 == start) => {}
                Some(_) => return Err(self.error(BencodeErrorKind::InvalidInteger)),
                None => return Err(self.error(BencodeErrorKind::Truncated)),
            }
        };
        let raw = &self.input[start..end];
        if !is_canonical_int(raw) {
            return Err(self.error(BencodeErrorKind::InvalidInteger));
        }
        let text =
            std::str::from_utf8(raw).map_err(|_| self.error(BencodeErrorKind::InvalidInteger))?;
        let n = text
            .parse::<i64>()
            .map_err(|_| self.error(BencodeErrorKind::InvalidInteger))?;
        Ok(Value::Int(n))
    }

    /// Parses `<len>:<bytes>`. The length is validated against the remaining
    /// input *before* any allocation, so a forged `999999999:` header can
    /// never balloon memory.
    fn byte_string(&mut self) -> Result<Cow<'a, [u8]>> {
        let start = self.pos;
        let Some(colon) = self.input[start..].iter().position(|&b| b == b':') else {
            return Err(self.error(BencodeErrorKind::Truncated));
        };
        let digits = &self.input[start..start + colon];
        if !is_canonical_len(digits) {
            return Err(self.error(BencodeErrorKind::InvalidInteger));
        }
        let text = std::str::from_utf8(digits)
            .map_err(|_| self.error(BencodeErrorKind::InvalidInteger))?;
        let len: usize = text
            .parse()
            .map_err(|_| self.error(BencodeErrorKind::InvalidInteger))?;
        let data_start = start + colon + 1;
        let Some(data_end) = data_start.checked_add(len) else {
            return Err(self.error(BencodeErrorKind::Truncated));
        };
        if data_end > self.input.len() {
            return Err(self.error(BencodeErrorKind::Truncated));
        }
        self.pos = data_end;
        Ok(Cow::Borrowed(&self.input[data_start..data_end]))
    }

    fn list(&mut self, depth: u32) -> Result<Value<'a>> {
        self.pos += 1; // consume 'l'
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Some(b'e') => {
                    self.pos += 1;
                    return Ok(Value::List(items));
                }
                Some(_) => items.push(self.value(depth + 1)?),
                None => return Err(self.error(BencodeErrorKind::Truncated)),
            }
        }
    }

    fn dict(&mut self, depth: u32) -> Result<Value<'a>> {
        self.pos += 1; // consume 'd'
        let mut map = BTreeMap::new();
        loop {
            match self.peek() {
                Some(b'e') => {
                    self.pos += 1;
                    return Ok(Value::Dict(map));
                }
                Some(b'0'..=b'9') => {
                    let key = self.byte_string()?;
                    let (value, span) = self.value_with_span(depth + 1)?;
                    if depth == 0 && self.track_entry_spans {
                        self.record_entry(key.clone(), span);
                    }
                    map.insert(key, value);
                }
                Some(_) => return Err(self.error(BencodeErrorKind::InvalidDictKey)),
                None => return Err(self.error(BencodeErrorKind::Truncated)),
            }
        }
    }

    /// Records a top-level entry's span; the last occurrence of a duplicated
    /// key wins, mirroring `BTreeMap::insert`.
    fn record_entry(&mut self, key: Cow<'a, [u8]>, span: Range<usize>) {
        match self
            .entry_spans
            .iter()
            .position(|(existing, _)| existing.as_ref() == key.as_ref())
        {
            Some(index) => self.entry_spans[index].1 = span,
            None => self.entry_spans.push((key, span)),
        }
    }
}

fn write_value(value: &Value<'_>, out: &mut Vec<u8>, depth: u32) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::Bencode {
            offset: out.len(),
            kind: BencodeErrorKind::DepthLimit,
        });
    }
    match value {
        Value::Int(i) => out.write_fmt(format_args!("i{i}e"))?,
        Value::Bytes(bytes) => {
            out.write_fmt(format_args!("{}:", bytes.len()))?;
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                write_value(item, out, depth + 1)?;
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            for (key, value) in map {
                out.write_fmt(format_args!("{}:", key.len()))?;
                out.extend_from_slice(key);
                write_value(value, out, depth + 1)?;
            }
            out.push(b'e');
        }
    }
    Ok(())
}

/// `i0e` is fine; `i03e`, `i-0e` and empty payloads are not.
fn is_canonical_int(raw: &[u8]) -> bool {
    match raw {
        [b'0'] => true,
        [] | [b'-'] => false,
        [b'-', rest @ ..] => {
            !rest.is_empty() && rest[0] != b'0' && rest.iter().all(u8::is_ascii_digit)
        }
        _ => raw[0] != b'0' && raw.iter().all(u8::is_ascii_digit),
    }
}

/// `<len>:` must have no leading zeros (`0:` alone is valid).
fn is_canonical_len(raw: &[u8]) -> bool {
    match raw {
        [b'0'] => true,
        [] | [b'0', ..] => false,
        _ => raw.iter().all(u8::is_ascii_digit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(b: &[u8]) -> Cow<'static, [u8]> {
        Cow::Owned(b.to_vec())
    }

    #[test]
    fn decodes_integers() {
        assert_eq!(decode(b"i42e").unwrap(), Value::Int(42));
        assert_eq!(decode(b"i-7e").unwrap(), Value::Int(-7));
        assert_eq!(decode(b"i0e").unwrap(), Value::Int(0));
        assert_eq!(
            decode(b"i9223372036854775807e").unwrap(),
            Value::Int(i64::MAX)
        );
        assert_eq!(
            decode(b"i-9223372036854775808e").unwrap(),
            Value::Int(i64::MIN)
        );
    }

    #[test]
    fn rejects_malformed_integers() {
        for bad in [
            "i",
            "i1",
            "ie",
            "i01e",
            "i-0e",
            "i1x2e",
            "i9223372036854775808e",
            "i 1e",
        ] {
            let err = decode(bad.as_bytes()).unwrap_err();
            assert!(matches!(err, Error::Bencode { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn decodes_byte_strings() {
        assert_eq!(decode(b"4:spam").unwrap(), Value::Bytes(owned(b"spam")));
        assert_eq!(decode(b"0:").unwrap(), Value::Bytes(owned(b"")));
        assert_eq!(
            decode(b"12:hello world!").unwrap(),
            Value::Bytes(owned(b"hello world!"))
        );
    }

    #[test]
    fn byte_strings_are_borrowed_not_copied() {
        let v = decode(b"4:spam").unwrap();
        match v {
            Value::Bytes(Cow::Borrowed(_)) => {}
            other => panic!("expected a borrowed byte string, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_byte_strings() {
        for bad in ["3:sp", "99:spam", "-3:spam", "03:spam", "4:spamx"] {
            let err = decode(bad.as_bytes()).unwrap_err();
            assert!(matches!(err, Error::Bencode { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn decodes_lists_and_dicts() {
        assert_eq!(decode(b"le").unwrap(), Value::List(vec![]));
        assert_eq!(decode(b"de").unwrap(), Value::Dict(BTreeMap::new()));

        let list = decode(b"l4:spami1ee").unwrap();
        assert_eq!(
            list,
            Value::List(vec![Value::Bytes(owned(b"spam")), Value::Int(1)])
        );

        let dict = decode(b"d3:bar4:spam3:fooi42ee").unwrap();
        assert_eq!(dict.get(b"bar").unwrap(), &Value::Bytes(owned(b"spam")));
        assert_eq!(dict.get(b"foo").unwrap(), &Value::Int(42));
        assert!(dict.get(b"nope").is_none());
    }

    #[test]
    fn rejects_invalid_containers() {
        for bad in [
            "l4:spam",
            "d3:foo",
            "d3:fooi1e2:ba",
            "di1ei0ee",
            "x",
            "",
            "i1ei2e",
        ] {
            let err = decode(bad.as_bytes()).unwrap_err();
            assert!(matches!(err, Error::Bencode { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn canonicalizes_dict_order_on_encode() {
        let v = decode(b"d2:zbi1e2:abi0ee").unwrap();
        let encoded = encode_to_vec(&v).unwrap();
        assert_eq!(encoded, b"d2:abi0e2:zbi1ee");
    }

    #[test]
    fn round_trips_values() {
        let cases: [&[u8]; 6] = [
            b"i-42e",
            b"0:",
            b"l1:a1:be",
            b"d1:ali1eee",
            b"d3:foo4:barse",
            b"d1:ad1:b1:cee",
        ];
        for case in cases {
            let v = decode(case).unwrap();
            assert_eq!(
                encode_to_vec(&v).unwrap(),
                case,
                "roundtrip failed for {case:?}"
            );
        }
    }

    #[test]
    fn enforces_depth_limit_on_decode() {
        let mut input = vec![b'l'; 700];
        input.extend_from_slice(&vec![b'e'; 700]);
        let err = decode(&input).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Bencode {
                    kind: BencodeErrorKind::DepthLimit,
                    ..
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn enforces_depth_limit_on_encode() {
        let mut deep = Value::List(vec![]);
        for _ in 0..700 {
            deep = Value::List(vec![deep]);
        }
        let err = encode_to_vec(&deep).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Bencode {
                    kind: BencodeErrorKind::DepthLimit,
                    ..
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn reports_error_offsets() {
        let err = decode(b"i1eX").unwrap_err();
        match err {
            Error::Bencode { offset, .. } => assert_eq!(offset, 3),
            other => panic!("expected bencode error, got {other:?}"),
        }
    }

    #[test]
    fn entry_spans_slice_the_raw_bytes() {
        let input = b"d1:ai1e4:infod4:name4:abcdee";
        let (value, spans) = decode_with_entry_spans(input).unwrap();
        let info_span = spans.get(&b"info"[..]).unwrap();
        assert_eq!(&input[info_span.clone()], b"d4:name4:abcde");
        // The span re-decodes to exactly the subtree stored under the key.
        assert_eq!(
            decode(&input[info_span]).unwrap(),
            *value.get(&b"info"[..]).unwrap()
        );
    }

    #[test]
    fn entry_spans_pin_exact_offsets() {
        let (_, spans) = decode_with_entry_spans(b"d1:ai1ee").unwrap();
        // d(0) "1:a"(1..4) "i1e"(4..7) e(7)
        assert_eq!(spans.get(b"a").unwrap(), 4..7);
    }

    #[test]
    fn spanned_decode_matches_plain_decode() {
        for case in [
            &b"d3:bar4:spam3:fooi42ee"[..],
            b"l1:a1:be",
            b"i-42e",
            b"d1:ad1:b1:cee",
        ] {
            let (value, _) = decode_with_entry_spans(case).unwrap();
            assert_eq!(value, decode(case).unwrap(), "mismatch for {case:?}");
        }
    }

    #[test]
    fn duplicate_keys_last_span_wins() {
        let input = b"d1:ki1e1:ki2ee";
        let (value, spans) = decode_with_entry_spans(input).unwrap();
        assert_eq!(value.get(b"k"), Some(&Value::Int(2)));
        assert_eq!(&input[spans.get(b"k").unwrap()], b"i2e");
        assert_eq!(spans.iter().count(), 1);
    }

    #[test]
    fn non_dict_top_level_yields_no_spans() {
        let (value, spans) = decode_with_entry_spans(b"i42e").unwrap();
        assert_eq!(value, Value::Int(42));
        assert!(spans.is_empty());
        let (_, spans) = decode_with_entry_spans(b"l1:ae").unwrap();
        assert!(spans.is_empty());
    }

    #[test]
    fn entry_spans_still_error_on_trailing_bytes() {
        let err = decode_with_entry_spans(b"d1:ai1eeX").unwrap_err();
        assert!(
            matches!(
                err,
                Error::Bencode {
                    kind: BencodeErrorKind::TrailingBytes,
                    ..
                }
            ),
            "got {err}"
        );
    }
}
