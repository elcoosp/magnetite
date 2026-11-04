//! `magnetite` — command-line front end for magnetite-core.
//!
//! Subcommands:
//!
//! - `bencode decode|encode` — inspect and produce canonical bencode
//! - `magnet parse` — break a magnet URI into typed fields
//! - `torrent info` — summarize a `.torrent` metainfo file
//!
//! `--json` switches every command to machine-readable output.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use magnetite_core::bencode::{self, Value};
use magnetite_core::infohash::InfoHash;
use magnetite_core::magnet as magnet_lib;
use magnetite_core::metainfo::{MetaInfo, TorrentInfo};

#[derive(Debug, Parser)]
#[command(
    name = "magnetite",
    version,
    about = "A magnet-first BitTorrent toolkit",
    propagate_version = true
)]
struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Encode/decode raw bencode.
    Bencode(BencodeArgs),
    /// Inspect magnet URIs.
    Magnet(MagnetArgs),
    /// Inspect `.torrent` metainfo files.
    Torrent(TorrentArgs),
}

#[derive(Debug, Args)]
struct BencodeArgs {
    #[command(subcommand)]
    command: BencodeCommand,
}

#[derive(Debug, Subcommand)]
enum BencodeCommand {
    /// Decode bencode into JSON (non-UTF-8 byte strings become {"$hex": "…"}).
    Decode {
        /// Input file; `-` or omitted reads stdin.
        file: Option<String>,
    },
    /// Encode JSON into canonical bencode ({"$hex": "…"} decodes to bytes).
    Encode {
        /// Input file; `-` or omitted reads stdin.
        file: Option<String>,
    },
}

#[derive(Debug, Args)]
struct MagnetArgs {
    #[command(subcommand)]
    command: MagnetCommand,
}

#[derive(Debug, Subcommand)]
enum MagnetCommand {
    /// Parse a magnet URI and print its fields.
    Parse {
        /// The magnet URI to parse.
        uri: String,
    },
}

#[derive(Debug, Args)]
struct TorrentArgs {
    #[command(subcommand)]
    command: TorrentCommand,
}

#[derive(Debug, Subcommand)]
enum TorrentCommand {
    /// Print a summary of a `.torrent` file.
    Info {
        /// Path to the `.torrent` file.
        file: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    tracing::debug!(?cli.command, "dispatching");

    match cli.command {
        Command::Bencode(args) => match args.command {
            BencodeCommand::Decode { file } => bencode_decode(file.as_deref(), cli.json),
            BencodeCommand::Encode { file } => bencode_encode(file.as_deref()),
        },
        Command::Magnet(args) => match args.command {
            MagnetCommand::Parse { uri } => magnet_parse(&uri, cli.json),
        },
        Command::Torrent(args) => match args.command {
            TorrentCommand::Info { file } => torrent_info(&file, cli.json),
        },
    }
}

fn read_input(path: Option<&str>) -> anyhow::Result<Vec<u8>> {
    match path {
        None | Some("-") => {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf).context("reading stdin")?;
            Ok(buf)
        }
        Some(path) => std::fs::read(path).with_context(|| format!("reading {path}")),
    }
}

fn value_to_json(value: &Value<'_>) -> serde_json::Value {
    match value {
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => serde_json::Value::from(s),
            Err(_) => serde_json::json!({ "$hex": hex::encode(b) }),
        },
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Dict(map) => {
            let mut obj = serde_json::Map::new();
            for (key, val) in map {
                let key = String::from_utf8_lossy(key).into_owned();
                obj.insert(key, value_to_json(val));
            }
            serde_json::Value::Object(obj)
        }
    }
}

fn json_to_bencode(value: &serde_json::Value) -> anyhow::Result<Value<'static>> {
    Ok(match value {
        serde_json::Value::Null => anyhow::bail!("JSON null has no bencode representation"),
        serde_json::Value::Bool(_) => anyhow::bail!("JSON booleans have no bencode representation"),
        serde_json::Value::Number(n) => Value::Int(
            n.as_i64()
                .ok_or_else(|| anyhow::anyhow!("integer out of i64 range: {n}"))?,
        ),
        serde_json::Value::String(s) => Value::Bytes(Cow::Owned(s.as_bytes().to_vec())),
        serde_json::Value::Array(items) => Value::List(
            items
                .iter()
                .map(json_to_bencode)
                .collect::<anyhow::Result<_>>()?,
        ),
        serde_json::Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (key, val) in map {
                if key == "$hex" {
                    if let serde_json::Value::String(s) = val {
                        let raw =
                            hex::decode(s).map_err(|e| anyhow::anyhow!("bad $hex value: {e}"))?;
                        out.insert(
                            Cow::Owned(key.as_bytes().to_vec()),
                            Value::Bytes(Cow::Owned(raw)),
                        );
                        continue;
                    }
                }
                let converted = json_to_bencode(val)?;
                out.insert(Cow::Owned(key.as_bytes().to_vec()), converted);
            }
            Value::Dict(out)
        }
    })
}

fn bencode_decode(file: Option<&str>, json: bool) -> anyhow::Result<()> {
    let input = read_input(file)?;
    let value = bencode::decode(&input)?;
    let json_value = value_to_json(&value);
    let rendered = if json {
        serde_json::to_string(&json_value)?
    } else {
        serde_json::to_string_pretty(&json_value)?
    };
    println!("{rendered}");
    Ok(())
}

fn bencode_encode(file: Option<&str>) -> anyhow::Result<()> {
    let input = read_input(file)?;
    let json_value: serde_json::Value =
        serde_json::from_slice(&input).context("input is not valid JSON")?;
    let value = json_to_bencode(&json_value)?;
    let encoded = bencode::encode_to_vec(&value)?;
    io::stdout().write_all(&encoded).context("writing stdout")?;
    Ok(())
}

fn magnet_parse(uri: &str, json: bool) -> anyhow::Result<()> {
    let magnet = magnet_lib::parse(uri)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&magnet_json(&magnet))?);
        return Ok(());
    }

    if let Some(name) = &magnet.display_name {
        println!("name      : {name}");
    }
    if let Some(hash) = &magnet.info_hash {
        println!("infohash  : {hash}");
    }
    if let Some(len) = magnet.length {
        println!("length    : {len}");
    }
    for tracker in &magnet.trackers {
        println!("tracker   : {tracker}");
    }
    for seed in &magnet.web_seeds {
        println!("web seed  : {seed}");
    }
    for peer in &magnet.peers {
        println!("peer      : {peer}");
    }
    for xt in &magnet.other_xt {
        println!("xt        : {xt}");
    }
    for (key, value) in &magnet.extra {
        println!("{key:<9} : {value}");
    }
    Ok(())
}

fn magnet_json(m: &magnet_lib::MagnetUri) -> serde_json::Value {
    serde_json::json!({
        "display_name": m.display_name,
        "info_hash": m.info_hash.map(InfoHash::to_hex),
        "trackers": m.trackers,
        "web_seeds": m.web_seeds,
        "peers": m.peers,
        "length": m.length,
        "other_xt": m.other_xt,
        "extra": m
            .extra
            .iter()
            .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
            .collect::<Vec<_>>(),
    })
}

fn torrent_info(file: &str, json: bool) -> anyhow::Result<()> {
    let raw = read_input(Some(file))?;
    let meta = MetaInfo::parse(&raw)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&torrent_json(&meta))?);
        return Ok(());
    }

    let (name, file_count) = match &meta.info {
        TorrentInfo::Single { name, .. } => (name.as_str(), 1),
        TorrentInfo::Multi { name, files, .. } => (name.as_str(), files.len()),
    };
    let total = meta.total_length();
    println!("name        : {name}");
    println!("infohash    : {}", meta.info_hash);
    println!("size        : {} ({total} bytes)", human_bytes(total));
    println!(
        "pieces      : {} × {}",
        meta.piece_count(),
        human_bytes(piece_len(&meta))
    );
    println!("files       : {file_count}");
    let tracker_count = meta.announce_list.iter().map(Vec::len).sum::<usize>()
        + usize::from(meta.announce.is_some());
    println!("trackers    : {tracker_count}");
    if let Some(by) = &meta.created_by {
        println!("created by  : {by}");
    }
    Ok(())
}

fn torrent_json(meta: &MetaInfo) -> serde_json::Value {
    serde_json::json!({
        "announce": meta.announce,
        "announce_list": meta.announce_list,
        "comment": meta.comment,
        "created_by": meta.created_by,
        "creation_date": meta.creation_date,
        "info_hash": meta.info_hash.to_hex(),
        "total_length": meta.total_length(),
        "piece_count": meta.piece_count(),
        "info": info_json(&meta.info),
    })
}

fn info_json(info: &TorrentInfo) -> serde_json::Value {
    match info {
        TorrentInfo::Single {
            name,
            length,
            piece_length,
            private,
            ..
        } => serde_json::json!({
            "type": "single",
            "name": name,
            "length": length,
            "piece_length": piece_length,
            "private": private,
        }),
        TorrentInfo::Multi {
            name,
            files,
            piece_length,
            private,
            ..
        } => serde_json::json!({
            "type": "multi",
            "name": name,
            "files": files
                .iter()
                .map(|f| serde_json::json!({ "length": f.length, "path": f.path }))
                .collect::<Vec<_>>(),
            "piece_length": piece_length,
            "private": private,
        }),
    }
}

fn piece_len(meta: &MetaInfo) -> u64 {
    match &meta.info {
        TorrentInfo::Single { piece_length, .. } | TorrentInfo::Multi { piece_length, .. } => {
            *piece_length
        }
    }
}

/// Display-only conversion; precision loss is fine for a summary line.
#[allow(clippy::cast_precision_loss)]
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
