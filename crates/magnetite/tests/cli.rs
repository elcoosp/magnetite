//! End-to-end CLI tests: spawn the real `magnetite` binary.

use std::borrow::Cow;

use assert_cmd::Command;
use magnetite_core::bencode::{self, Value};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn cmd() -> Command {
    Command::cargo_bin("magnetite").expect("binary builds")
}

fn bytes(b: &'static [u8]) -> Value<'static> {
    Value::Bytes(Cow::Borrowed(b))
}

fn fixture_torrent() -> Vec<u8> {
    let info = Value::Dict(
        [
            (&b"length"[..], Value::Int(1024)),
            (&b"name"[..], bytes(b"data.bin")),
            (&b"piece length"[..], Value::Int(16384)),
            (&b"pieces"[..], Value::Bytes(Cow::Owned(vec![0u8; 20]))),
        ]
        .into_iter()
        .map(|(k, v)| (Cow::Borrowed(k), v))
        .collect(),
    );
    let root = Value::Dict([(Cow::Borrowed(&b"info"[..]), info)].into_iter().collect());
    bencode::encode_to_vec(&root).unwrap()
}

#[test]
fn decodes_bencode_to_json() {
    cmd()
        .args(["bencode", "decode"])
        .write_stdin("d3:bar4:spam3:fooi42ee")
        .assert()
        .success()
        .stdout(contains("\"bar\": \"spam\"").and(contains("\"foo\": 42")));
}

#[test]
fn encodes_json_to_bencode() {
    cmd()
        .args(["bencode", "encode"])
        .write_stdin(r#"{"foo": [1, "two"]}"#)
        .assert()
        .success()
        .stdout(contains("3:fooli1e3:twoe"));
}

#[test]
fn encodes_hex_bytes_back_to_raw() {
    cmd()
        .args(["bencode", "encode"])
        .write_stdin(r#"{"$hex": "0a0b"}"#)
        .assert()
        .success()
        .stdout(contains("2:\n\x0b"));
}

#[test]
fn rejects_garbage_with_error_and_nonzero_exit() {
    cmd()
        .args(["bencode", "decode"])
        .write_stdin("xx")
        .assert()
        .failure()
        .stderr(contains("invalid bencode"));
}

#[test]
fn parses_magnet_uri() {
    cmd()
        .args([
            "magnet",
            "parse",
            "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=Ubuntu&tr=udp%3A%2F%2Ft%3A80",
        ])
        .assert()
        .success()
        .stdout(
            contains("infohash  : c12fe1c06bba254a9dc9f519b335aa7c1367a88a")
                .and(contains("tracker   : udp://t:80"))
                .and(contains("name      : Ubuntu")),
        );
}

#[test]
fn prints_torrent_info() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.torrent");
    std::fs::write(&path, fixture_torrent()).unwrap();

    cmd()
        .args(["torrent", "info"])
        .arg(&path)
        .assert()
        .success()
        .stdout(
            contains("name        : data.bin")
                .and(contains("size        : 1.0 KiB"))
                .and(contains("infohash    : ")),
        );
}

#[test]
fn json_output_is_valid_json() {
    let out = cmd()
        .args([
            "--json",
            "magnet",
            "parse",
            "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        parsed["info_hash"],
        "c12fe1c06bba254a9dc9f519b335aa7c1367a88a"
    );
}

#[test]
fn torrent_info_json_includes_structure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.torrent");
    std::fs::write(&path, fixture_torrent()).unwrap();

    let out = cmd()
        .args(["--json", "torrent", "info"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["info"]["type"], "single");
    assert_eq!(parsed["info"]["name"], "data.bin");
    assert_eq!(parsed["total_length"], 1024);
}
