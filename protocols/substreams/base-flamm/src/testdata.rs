// Copyright (c) 2026 Everlong Labs Limited
//! Real Base data replayed through the modules: the fixtures in `testdata/` (see
//! `testdata/README.md`) and the synthetic `Block`s built from them.
use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    sync::OnceLock,
};

use serde_json::{json, Map, Value};
use substreams_ethereum::pb::eth::v2::{
    block::DetailLevel, Block, BlockHeader, Call, CallType, CodeChange, Log, StorageChange,
    TransactionReceipt, TransactionTrace, TransactionTraceStatus,
};

use crate::flamm::{
    feeds::{self, FeedKind},
    keys::{self, parse_address, parse_word, Address, Word},
};

/// `testdata/e2e_blocks.json.gz`: the tracked words at 51154965 (`seed`) and one stage per block
/// of interest, in order (`testdata/README.md`). The package's one corpus of real Base data.
pub fn e2e_blocks() -> &'static Value {
    static BLOCKS: OnceLock<Value> = OnceLock::new();
    BLOCKS.get_or_init(|| {
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(include_bytes!("../testdata/e2e_blocks.json.gz").as_slice())
            .read_to_end(&mut raw)
            .expect("gzip");
        serde_json::from_slice(&raw).expect("e2e_blocks.json")
    })
}

fn hex_of(v: &Value) -> String {
    v.as_str()
        .expect("hex string")
        .to_string()
}

/// One block of interest cut out of `e2e_blocks.json.gz`: its header, the transaction of interest
/// (`tx`, with the `transactionIndex` spelling `fixture_block` reads) and its receipt logs, the
/// block's own diff over the tracked words (`storage_diffs`), the runtime code and codehash of
/// every contract deployed up to it (`codes`, `codehashes`) and the two views of the words the
/// block opens on:
///
/// * `store_before`, every tracked word written since 51154965 — the words store as the package
///   holds it entering the block. A word it lacks was never written, so it reads as the manifest
///   seed and then as zero, which is what a `WordView` does with it.
/// * `words_before`, the seed at 51154965 with those writes applied — what `eth_getStorageAt` at
///   the parent block answers, zero-valued words included.
pub fn stage(block: u64) -> Value {
    let fx = e2e_blocks();
    let seed: BTreeMap<(String, String), String> = fx["seed"]
        .as_array()
        .expect("seed")
        .iter()
        .map(|w| ((hex_of(&w["address"]), hex_of(&w["slot"])), hex_of(&w["value"])))
        .collect();
    let mut store: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut codes = Map::new();
    let mut found = None;
    for st in fx["stages"].as_array().expect("stages") {
        let number = st["block"]
            .as_u64()
            .expect("stage block");
        if number > block {
            break;
        }
        for w in st["catchup_writes"]
            .as_array()
            .into_iter()
            .flatten()
        {
            store.insert((hex_of(&w["address"]), hex_of(&w["slot"])), hex_of(&w["new"]));
        }
        if let Some(c) = st["codes"].as_object() {
            codes.extend(
                c.iter()
                    .map(|(a, v)| (a.clone(), v.clone())),
            );
        }
        if number == block {
            found = Some(st);
            break;
        }
        for w in st["writes"]
            .as_array()
            .into_iter()
            .flatten()
        {
            store.insert((hex_of(&w["address"]), hex_of(&w["slot"])), hex_of(&w["new"]));
        }
    }
    let mut words = seed;
    words.extend(
        store
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    let rows = |m: BTreeMap<(String, String), String>| {
        m.into_iter()
            .map(|((address, slot), value)| json!({"address": address, "slot": slot, "value": value}))
            .collect::<Vec<_>>()
    };
    let st = found.unwrap_or_else(|| panic!("no stage for block {block}"));
    let tx = &st["tx"];
    json!({
        "block": block,
        "header": st["header"],
        "tx": {
            "hash": tx["hash"],
            "from": tx["from"],
            "to": tx["to"],
            "input": tx["input"],
            "transactionIndex": format!("0x{:x}", tx["index"].as_u64().expect("tx index")),
        },
        "logs": tx["logs"],
        "storage_diffs": st["writes"],
        "store_before": rows(store),
        "words_before": rows(words),
        "codehashes": codes
            .iter()
            .map(|(a, c)| (a.clone(), c["codehash"].clone()))
            .collect::<Map<String, Value>>(),
        "codes": codes,
    })
}

pub fn fixture(name: &str) -> Value {
    let raw = match name {
        "creation" => return stage(51_154_990),
        "activation" => return stage(51_298_416),
        "swap" => return stage(51_302_916),
        "seeds" => include_str!("../testdata/seeds_51154965.json"),
        "feed_logs" => include_str!("../testdata/feed_logs.json"),
        "snapshot" => include_str!("../testdata/snapshot_51302915.json"),
        "immutables" => include_str!("../testdata/immutables_51154990.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(raw).expect("fixture json")
}

pub fn hex_bytes(v: &Value) -> Vec<u8> {
    let s = v.as_str().expect("hex string");
    hex::decode(s.strip_prefix("0x").unwrap_or(s)).expect("hex")
}

pub fn hex_u64(v: &Value) -> u64 {
    let s = v.as_str().expect("hex string");
    u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).expect("hex u64")
}

pub fn address(v: &Value) -> Address {
    parse_address(v.as_str().expect("address")).expect("address")
}

pub fn word(v: &Value) -> Word {
    parse_word(v.as_str().expect("word")).expect("word")
}

/// `(address, slot) -> value` from a fixture's word list.
pub fn words_map(list: &Value) -> HashMap<(Address, Word), Word> {
    list.as_array()
        .expect("array")
        .iter()
        .map(|w| ((address(&w["address"]), word(&w["slot"])), word(&w["value"])))
        .collect()
}

pub fn log(ordinal: u64, v: &Value) -> Log {
    Log {
        address: hex_bytes(&v["address"]),
        topics: v["topics"]
            .as_array()
            .expect("topics")
            .iter()
            .map(hex_bytes)
            .collect(),
        data: hex_bytes(&v["data"]),
        index: hex_u64(&v["logIndex"]) as u32,
        block_index: hex_u64(&v["logIndex"]) as u32,
        ordinal,
    }
}

/// Storage changes from a fixture's `storage_diffs` (`old` -> `new` per slot).
pub fn storage_changes(diffs: &Value, first_ordinal: u64) -> Vec<StorageChange> {
    diffs
        .as_array()
        .expect("diffs")
        .iter()
        .enumerate()
        .map(|(i, d)| StorageChange {
            address: hex_bytes(&d["address"]),
            key: hex_bytes(&d["slot"]),
            old_value: hex_bytes(&d["old"]),
            new_value: hex_bytes(&d["new"]),
            ordinal: first_ordinal + i as u64,
        })
        .collect()
}

pub struct TxSpec {
    pub index: u32,
    pub hash: Vec<u8>,
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub input: Vec<u8>,
    pub logs: Vec<Log>,
    pub storage_changes: Vec<StorageChange>,
    pub code_changes: Vec<CodeChange>,
    pub create: bool,
}

pub fn transaction(spec: TxSpec) -> TransactionTrace {
    let call = Call {
        index: 1,
        parent_index: 0,
        depth: 0,
        call_type: if spec.create { CallType::Create } else { CallType::Call } as i32,
        caller: spec.from.clone(),
        address: spec.to.clone(),
        input: spec.input.clone(),
        logs: spec.logs.clone(),
        storage_changes: spec.storage_changes,
        code_changes: spec.code_changes,
        state_reverted: false,
        ..Default::default()
    };
    TransactionTrace {
        to: spec.to,
        input: spec.input,
        index: spec.index,
        hash: spec.hash,
        from: spec.from,
        status: TransactionTraceStatus::Succeeded as i32,
        receipt: Some(TransactionReceipt { logs: spec.logs, ..Default::default() }),
        calls: vec![call],
        ..Default::default()
    }
}

pub fn block(
    number: u64,
    timestamp: u64,
    hash: Vec<u8>,
    parent_hash: Vec<u8>,
    txs: Vec<TransactionTrace>,
) -> Block {
    Block {
        hash: hash.clone(),
        number,
        header: Some(BlockHeader {
            parent_hash,
            number,
            hash,
            timestamp: Some(prost_types::Timestamp { seconds: timestamp as i64, nanos: 0 }),
            ..Default::default()
        }),
        transaction_traces: txs,
        detail_level: DetailLevel::DetaillevelExtended as i32,
        ..Default::default()
    }
}

/// A block from a `stage` fixture: its one transaction with the stage's logs and storage diffs.
pub fn fixture_block(f: &Value, extra_code_changes: Vec<CodeChange>) -> Block {
    let tx = &f["tx"];
    let logs = f["logs"]
        .as_array()
        .expect("logs")
        .iter()
        .enumerate()
        .map(|(i, l)| log(1000 + i as u64, l))
        .collect();
    let spec = TxSpec {
        index: hex_u64(&tx["transactionIndex"]) as u32,
        hash: hex_bytes(&tx["hash"]),
        from: hex_bytes(&tx["from"]),
        to: hex_bytes(&tx["to"]),
        input: hex_bytes(&tx["input"]),
        logs,
        storage_changes: storage_changes(&f["storage_diffs"], 1),
        code_changes: extra_code_changes,
        create: false,
    };
    block(
        f["block"].as_u64().expect("block"),
        hex_u64(&f["header"]["timestamp"]),
        hex_bytes(&f["header"]["hash"]),
        hex_bytes(&f["header"]["parentHash"]),
        vec![transaction(spec)],
    )
}

pub fn code_change(address: &Address, code: &[u8], ordinal: u64) -> CodeChange {
    CodeChange {
        address: address.to_vec(),
        new_code: code.to_vec(),
        new_hash: crate::flamm::keys::keccak256(code).to_vec(),
        ordinal,
        ..Default::default()
    }
}

/// The transmission words of every round of a `DualAggregator` window (`latest-20..=latest` and
/// the secondary round), so a replay has a full ring to evict from: the seeds carry the ring of
/// block 51154965, the fixture rounds are later. Round `r` packs answer `r`, observations
/// timestamp `r`, recorded timestamp `r + 1`.
pub fn synthetic_ring(
    words: &mut HashMap<(Address, Word), Word>,
    agg: &Address,
    latest: u32,
    secondary: u32,
) {
    for r in feeds::dual_ring(latest, secondary) {
        let packed = feeds::pack_transmission(&keys::word_from_u64(r as u64), r, r + 1);
        words.insert((*agg, FeedKind::Dual.transmission(r).unwrap()), packed);
    }
}
