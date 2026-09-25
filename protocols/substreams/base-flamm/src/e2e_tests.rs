// Copyright (c) 2026 Everlong Labs Limited
//! The package end to end, without the hosted range harness: real Base blocks assembled from
//! `eth_getStorageAt` / `eth_getTransactionReceipt` / `eth_getCode` (`testdata/e2e_blocks.json.gz`,
//! provenance in `testdata/README.md`) are fed to every module core in the manifest's order, the
//! stores are kept as the substreams engine keeps them, and the emitted components, attributes
//! and balances are folded into the rows the indexer holds (`tycho-storage`'s rule) and into the
//! per-block messages the client delivers (`ProtocolComponentStateDelta::merge`). The fold is
//! compared, block by block, with the committed stream fixture the `flamm` `ProtocolSim` of
//! tycho-simulation replays (`crates/tycho-simulation/src/evm/protocol/flamm/testdata/snapshots/
//! e2e_stream.json.gz`), so the simulation tests quote from exactly what this package emits.
//!
//! The blocks: the creator's deployments (51154978-51154988, `store_deployments`), the pool's
//! creation (51154990), the range test's first stop block (51155010), the curator's activation
//! (51298416), every swap the pool has settled (51302916, 51343234, 51347390, 51420672,
//! 51420867, 51430828), two of its six deposits (51300667, 51426394), one of its four
//! withdrawals (51348093), one of the keeper's recenters (51384803), the range test's second
//! stop block (51302920), three recent blocks each carrying a Chainlink round (USDC/USD
//! 51429815, cbBTC/USD 51433135, the Morpho oracle's `DualAggregator` 51433218), the
//! `LevPauseSet(false)` that unpaused leverage (51433699), the `MaxSpreadAgeSet(0)` that cleared
//! the spread's staleness window and so opened the lever-up venue (51649706) and a later block
//! at which both venues quote (51670000). Between two blocks of interest the net change of every
//! tracked word is fed as one synthetic transaction at the block before the
//! next one of interest: the words store and the rows are functions of the words' values, so
//! the fold after that transaction is the fold the real stream reaches after the same blocks.
//! The other deposits (51343943, 51347236, 51347413, 51353593), withdrawals (51344221,
//! 51347256, 51353599) and keeper transactions are carried this way: their net effect reaches
//! the fold, their own blocks are not replayed. A block of interest carries its own diff as its
//! own transaction.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Read,
};

use serde_json::{json, Value};
use substreams_ethereum::pb::eth::v2::{Block, Log, StorageChange, TransactionTrace};
use tycho_substreams::prelude::{BlockChanges, ChangeType, ProtocolComponent};

use crate::{
    config::Config,
    flamm::{
        feeds,
        keys::{self, hex_address, hex_word, parse_address, parse_word, Address, Word},
        statics, PoolConfig, Role,
    },
    modules::{components_in_block, protocol_changes, tracked_writes},
    testdata::{self, hex_bytes, hex_u64},
};

const POOL: &str = "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572";
const CREATION_BLOCK: u64 = 51_154_990;
const CREATION_TX: &str = "0x783b464e93692538bde6dc1b53b959087f1e0ba2af3b6bc55ff13a449101d1e0";

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("gzip");
    out
}

/// The stream fixture the simulation replays, committed beside the ProtocolSim's other fixtures.
fn committed_stream() -> Value {
    serde_json::from_slice(&gunzip(include_bytes!(
        "../../../../crates/tycho-simulation/src/evm/protocol/flamm/testdata/snapshots/e2e_stream.json.gz"
    )))
    .expect("e2e_stream.json")
}

fn manifest_params() -> String {
    let manifest = include_str!("../base-flamm.yaml");
    let block = manifest
        .split_once("&params >-\n")
        .expect("params anchor")
        .1;
    // The folded scalar as YAML folds it: every line of the block, joined by one space.
    block
        .lines()
        .take_while(|l| l.starts_with("    "))
        .map(|l| &l[4..])
        .collect::<Vec<_>>()
        .join(" ")
}

fn config() -> Config {
    Config::parse(&manifest_params()).expect("manifest params parse")
}

fn word_of(v: &Value) -> Word {
    parse_word(v.as_str().expect("word")).expect("word")
}

fn address_of(v: &Value) -> Address {
    parse_address(v.as_str().expect("address")).expect("address")
}

/// `{address, slot, value}` rows as a word map.
fn word_map(list: &Value) -> HashMap<(Address, Word), Word> {
    list.as_array()
        .expect("word rows")
        .iter()
        .map(|w| ((address_of(&w["address"]), word_of(&w["slot"])), word_of(&w["value"])))
        .collect()
}

/// A stage's `writes` (`{address, slot, old, new}`) as storage changes from `first_ordinal`.
fn writes(list: &Value, first_ordinal: u64) -> Vec<StorageChange> {
    list.as_array()
        .expect("writes")
        .iter()
        .enumerate()
        .map(|(i, w)| StorageChange {
            address: hex_bytes(&w["address"]),
            key: hex_bytes(&w["slot"]),
            old_value: w["old"]
                .as_str()
                .map(|s| hex::decode(s.trim_start_matches("0x")).unwrap())
                .unwrap_or_default(),
            new_value: hex_bytes(&w["new"]),
            ordinal: first_ordinal + i as u64,
        })
        .collect()
}

fn logs_of(list: &Value) -> Vec<Log> {
    list.as_array()
        .map(|logs| {
            logs.iter()
                .enumerate()
                .map(|(i, l)| testdata::log(1000 + i as u64, l))
                .collect()
        })
        .unwrap_or_default()
}

fn header_block(header: &Value, txs: Vec<TransactionTrace>) -> Block {
    testdata::block(
        hex_u64(&header["number"]),
        hex_u64(&header["timestamp"]),
        hex_bytes(&header["hash"]),
        hex_bytes(&header["parentHash"]),
        txs,
    )
}

/// A synthetic block carrying the net diff of the blocks between two stages as one transaction:
/// index 0, zero hash and parties, at the height and header the diff is valued at.
fn catchup_block(header: &Value, diff: &Value) -> Block {
    let tx = testdata::transaction(testdata::TxSpec {
        index: 0,
        hash: vec![0; 32],
        from: vec![0; 20],
        to: vec![0; 20],
        input: vec![],
        logs: vec![],
        storage_changes: writes(diff, 1),
        code_changes: vec![],
        create: false,
    });
    header_block(header, vec![tx])
}

/// The stage's own block: its transaction of interest carrying the block's storage diff, its
/// receipt logs and the runtime code of the contracts it created.
fn stage_block(stage: &Value) -> Block {
    let tx = &stage["tx"];
    let mut code_changes = Vec::new();
    if let Some(codes) = stage["codes"].as_object() {
        for (i, (address, c)) in codes.iter().enumerate() {
            let code = hex_bytes(&c["code"]);
            assert_eq!(keys::keccak256(&code).to_vec(), hex_bytes(&c["codehash"]), "{address}");
            code_changes.push(testdata::code_change(
                &parse_address(address).unwrap(),
                &code,
                500 + i as u64,
            ));
        }
    }
    let create = tx["to"].is_null();
    let spec = testdata::TxSpec {
        index: tx["index"].as_u64().expect("tx index") as u32,
        hash: hex_bytes(&tx["hash"]),
        from: hex_bytes(&tx["from"]),
        to: if create { vec![] } else { hex_bytes(&tx["to"]) },
        input: hex_bytes(&tx["input"]),
        logs: logs_of(&tx["logs"]),
        storage_changes: writes(&stage["writes"], 1),
        code_changes,
        create,
    };
    header_block(&stage["header"], vec![testdata::transaction(spec)])
}

/// One component as the indexer holds it: the component row, its attribute rows and balances.
#[derive(Clone, Debug, Default)]
struct Held {
    component: Option<ProtocolComponent>,
    creation_tx: Vec<u8>,
    created_in: u64,
    rows: BTreeMap<String, Vec<u8>>,
    balances: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// The per-block message a component receives from the client, merged over the block's
/// transactions like `ProtocolComponentStateDelta::merge` (`tycho-common`
/// `models/protocol.rs`): a later deletion drops an earlier update of the same attribute, a
/// later update drops an earlier deletion, values are last-wins.
#[derive(Clone, Debug, Default, PartialEq)]
struct Delta {
    updated: BTreeMap<String, Vec<u8>>,
    deleted: BTreeSet<String>,
    balances: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Delta {
    fn merge(&mut self, updated: BTreeMap<String, Vec<u8>>, deleted: BTreeSet<String>) {
        for name in &deleted {
            self.updated.remove(name);
        }
        for name in updated.keys() {
            self.deleted.remove(name);
        }
        self.updated.extend(updated);
        self.deleted.extend(deleted);
    }
}

/// One block of the client stream: the components created in it with their snapshot (the rows
/// after the block, what the RPC would serve a client that snapshots them there) and the deltas
/// of the components that existed before it.
#[derive(Clone, Debug, Default)]
struct StreamBlock {
    number: u64,
    timestamp: u64,
    hash: Vec<u8>,
    parent_hash: Vec<u8>,
    new: BTreeMap<String, Held>,
    deltas: BTreeMap<String, Delta>,
}

/// The indexer and the substreams stores, driven by the module cores in the manifest's order:
/// `store_deployments` (set-if-not-exists, read at its end-of-block state by the maps),
/// `store_words` (set, read as of the start of the block: the maps layer the block's own writes
/// through `WordView`), `map_components`, `store_pools` (append, read as of the start of the
/// block), `map_protocol_changes`; then the fold of the emitted changes into the rows
/// (`tycho-storage`: creation/update inserts or replaces, a deletion must find its row) and
/// into the client's per-block messages.
struct Indexer {
    cfg: Config,
    deployments: HashMap<Address, (Role, Word)>,
    words: HashMap<(Address, Word), Word>,
    pools: Vec<PoolConfig>,
    held: BTreeMap<String, Held>,
}

impl Indexer {
    fn new(cfg: Config) -> Self {
        Self {
            cfg,
            deployments: HashMap::new(),
            words: HashMap::new(),
            pools: Vec::new(),
            held: BTreeMap::new(),
        }
    }

    fn process(&mut self, block: &Block) -> (BlockChanges, StreamBlock) {
        // store_deployments: every registered code created in the block, first write wins.
        for d in crate::flamm::words::deployments_in_block(block, &self.cfg.deployments) {
            self.deployments
                .entry(d.address)
                .or_insert((d.role, d.codehash));
        }
        let deployments = self.deployments.clone();
        // store_words: the block's tracked writes, applied after the maps ran (they read the
        // store as of the start of the block).
        let block_writes =
            tracked_writes(block, &self.cfg, |a| deployments.get(a).map(|(r, _)| *r));
        let words = self.words.clone();
        let first_word = |a: &Address, k: &Word| words.get(&(*a, *k)).copied();
        let deployment = |a: &Address| deployments.get(a).copied();
        let components = components_in_block(block, &self.cfg, &deployment, first_word);
        let changes =
            protocol_changes(block, &self.cfg, self.pools.clone(), &components, first_word);
        for w in block_writes {
            self.words
                .insert((w.address, w.key), w.value);
        }
        for tx in &components.tx_components {
            for c in &tx.components {
                let kind = c
                    .get_attribute_value("component_kind")
                    .unwrap_or_default();
                if keys::is_zero(&kind) {
                    let cfg = statics::pool_config_from_component(c).expect("pool config");
                    if !self
                        .pools
                        .iter()
                        .any(|p| p.pool == cfg.pool)
                    {
                        self.pools.push(cfg);
                    }
                }
            }
        }
        let stream = self.fold(block, &changes);
        (changes, stream)
    }

    /// Folds one block's changes into the rows and builds the block's client message.
    fn fold(&mut self, block: &Block, changes: &BlockChanges) -> StreamBlock {
        let header = block.header.as_ref().unwrap();
        let mut out = StreamBlock {
            number: block.number,
            timestamp: header
                .timestamp
                .as_ref()
                .map(|t| t.seconds as u64)
                .unwrap_or_default(),
            hash: block.hash.clone(),
            parent_hash: header.parent_hash.clone(),
            ..Default::default()
        };
        let mut sorted: Vec<_> = changes.changes.iter().collect();
        sorted.sort_by_key(|c| c.tx.as_ref().map(|t| t.index));
        let mut created_here: BTreeSet<String> = BTreeSet::new();
        for tx in sorted {
            let tx_hash = tx
                .tx
                .as_ref()
                .map(|t| t.hash.clone())
                .unwrap_or_default();
            for c in &tx.component_changes {
                assert_eq!(c.change, i32::from(ChangeType::Creation), "{}", c.id);
                assert!(!self.held.contains_key(&c.id), "{} created twice", c.id);
                self.held.insert(
                    c.id.clone(),
                    Held {
                        component: Some(c.clone()),
                        creation_tx: tx_hash.clone(),
                        created_in: block.number,
                        ..Default::default()
                    },
                );
                created_here.insert(c.id.clone());
            }
            for e in &tx.entity_changes {
                let held = self
                    .held
                    .get_mut(&e.component_id)
                    .unwrap_or_else(|| panic!("entity change for unknown {}", e.component_id));
                let mut updated = BTreeMap::new();
                let mut deleted = BTreeSet::new();
                for a in &e.attributes {
                    let at =
                        format!("block {} tx {:?}", block.number, tx.tx.as_ref().map(|t| t.index));
                    if a.change == i32::from(ChangeType::Deletion) {
                        assert!(
                            held.rows.remove(&a.name).is_some(),
                            "{at}: Deletion of `{}`, a row the indexer does not hold",
                            a.name
                        );
                        deleted.insert(a.name.clone());
                        continue;
                    }
                    // The indexer's revert path restores an `Update` from the row's prior value
                    // and deletes a `Creation` (`protocol_extractor.rs`, `AttrRevert`): the
                    // change type must say whether the row exists.
                    if a.change == i32::from(ChangeType::Creation) {
                        assert!(
                            !held.rows.contains_key(&a.name),
                            "{at}: Creation of `{}`, a row the indexer already holds",
                            a.name
                        );
                    } else {
                        assert!(
                            held.rows.contains_key(&a.name),
                            "{at}: Update of `{}`, a row the indexer does not hold",
                            a.name
                        );
                    }
                    held.rows
                        .insert(a.name.clone(), a.value.clone());
                    updated.insert(a.name.clone(), a.value.clone());
                }
                if !created_here.contains(&e.component_id) {
                    out.deltas
                        .entry(e.component_id.clone())
                        .or_default()
                        .merge(updated, deleted);
                }
            }
            for b in &tx.balance_changes {
                let id = String::from_utf8(b.component_id.clone()).unwrap();
                let held = self
                    .held
                    .get_mut(&id)
                    .unwrap_or_else(|| panic!("balance change for unknown {id}"));
                held.balances
                    .insert(b.token.clone(), b.balance.clone());
                if !created_here.contains(&id) {
                    out.deltas
                        .entry(id)
                        .or_default()
                        .balances
                        .insert(b.token.clone(), b.balance.clone());
                }
            }
        }
        for id in created_here {
            out.new
                .insert(id.clone(), self.held[&id].clone());
        }
        out
    }

    /// `feed_state` of the words store for every feed of every pool: what the rows of a feed
    /// must be after every block.
    fn check_feed_rows(&self, block: u64) {
        for pool in &self.pools {
            for id in pool.component_ids() {
                let rows = &self.held[&id].rows;
                for feed in &pool.feeds {
                    // The store, else the manifest seed: what `WordView` answers the maps.
                    let derived = feeds::feed_state(
                        &feed.role,
                        &feed.proxy,
                        &self.cfg.aggregators,
                        |a, k| {
                            self.words
                                .get(&(*a, *k))
                                .or_else(|| self.cfg.words.get(&(*a, *k)))
                                .copied()
                        },
                    );
                    let prefix = format!("feed:{}:", feed.role);
                    let held: BTreeMap<String, Vec<u8>> = rows
                        .iter()
                        .filter(|(k, _)| {
                            k.starts_with(&prefix) && !k.ends_with(":access_controller")
                        })
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    assert_eq!(held, derived, "block {block}: feed {} rows of {id}", feed.role);
                }
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

impl StreamBlock {
    /// The block as the stream fixture records it: the new components' snapshots (as
    /// `ComponentWithState`: the component, its rows and balances) and the deltas.
    fn to_json(&self) -> Value {
        let attrs = |m: &BTreeMap<String, Vec<u8>>| -> Value {
            m.iter()
                .map(|(k, v)| (k.clone(), Value::String(hex(v))))
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        let balances = |m: &BTreeMap<Vec<u8>, Vec<u8>>| -> Value {
            m.iter()
                .map(|(k, v)| (hex(k), Value::String(hex(v))))
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        let new: serde_json::Map<String, Value> = self
            .new
            .iter()
            .map(|(id, held)| {
                let c = held.component.as_ref().unwrap();
                let statics: serde_json::Map<String, Value> = c
                    .static_att
                    .iter()
                    .map(|a| (a.name.clone(), Value::String(hex(&a.value))))
                    .collect();
                (
                    id.clone(),
                    json!({
                        "component": {
                            "id": c.id,
                            "protocol_system": "flamm",
                            "protocol_type_name": c.protocol_type.as_ref().unwrap().name,
                            "tokens": c.tokens.iter().map(|t| hex(t)).collect::<Vec<_>>(),
                            "contract_ids": c.contracts.iter().map(|t| hex(t)).collect::<Vec<_>>(),
                            "static_attributes": statics,
                            "creation_tx": hex(&held.creation_tx),
                            "created_at_block": held.created_in,
                        },
                        "attributes": attrs(&held.rows),
                        "balances": balances(&held.balances),
                    }),
                )
            })
            .collect();
        let deltas: serde_json::Map<String, Value> = self
            .deltas
            .iter()
            .map(|(id, d)| {
                (
                    id.clone(),
                    json!({
                        "updated_attributes": attrs(&d.updated),
                        "deleted_attributes": d.deleted.iter().cloned().collect::<Vec<_>>(),
                        "balances": balances(&d.balances),
                    }),
                )
            })
            .collect();
        json!({
            "number": self.number,
            "timestamp": self.timestamp,
            "hash": hex(&self.hash),
            "parent_hash": hex(&self.parent_hash),
            "new_components": new,
            "deltas": deltas,
        })
    }
}

/// One stage of the replay: the block fed and what the fixture says about it.
struct Replayed {
    stage: Value,
    stream: Vec<StreamBlock>,
}

/// Feeds every stage to the indexer, checking the storage rule and the feed rows after every
/// block, and returns the indexer with the stream and the stages in order.
fn replay() -> (Indexer, Vec<Replayed>) {
    let fx = testdata::e2e_blocks();
    let cfg = config();
    let seed = word_map(&fx["seed"]);
    // The universe at initialBlock - 1 is the manifest's `words`: every seeded word has the
    // seeded value and nothing else is non-zero.
    for (key, value) in &cfg.words {
        assert_eq!(seed.get(key), Some(value), "seed {} {}", hex_address(&key.0), hex_word(&key.1));
    }
    for (key, value) in &seed {
        if !keys::is_zero(value) {
            assert!(cfg.words.contains_key(key), "unseeded non-zero word at 51154965");
        }
    }
    let mut idx = Indexer::new(cfg);
    let mut out = Vec::new();
    for stage in fx["stages"].as_array().unwrap() {
        let mut stream = Vec::new();
        let kind = stage["kind"].as_str().unwrap();
        let number = stage["block"].as_u64().unwrap();
        let mut blocks = Vec::new();
        if kind == "catchup" {
            blocks.push(catchup_block(&stage["header"], &stage["writes"]));
        } else {
            if !stage["catchup_writes"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                blocks.push(catchup_block(&stage["header_before"], &stage["catchup_writes"]));
            }
            blocks.push(stage_block(stage));
        }
        for block in &blocks {
            let (changes, sb) = idx.process(block);
            assert_eq!(changes.block.as_ref().unwrap().number, block.number);
            idx.check_feed_rows(block.number);
            for pool in &idx.pools {
                let [swap, lever] = pool.component_ids();
                assert_eq!(idx.held[&swap].rows, idx.held[&lever].rows, "block {}", block.number);
                assert_eq!(idx.held[&swap].balances, idx.held[&lever].balances);
            }
            stream.push(sb);
        }
        assert_eq!(stream.last().unwrap().number, number);
        // At a pinned block the words store holds exactly the chain's values of every tracked
        // word (`state_after`, `eth_getStorageAt` at the block): a word the store lacks reads
        // as zero for a FLAMM-owned contract and as the seed otherwise.
        if let Some(state) = stage.get("state_after") {
            for (key, value) in word_map(state) {
                let held = idx
                    .words
                    .get(&key)
                    .copied()
                    .or_else(|| idx.cfg.words.get(&key).copied())
                    .unwrap_or([0u8; 32]);
                assert_eq!(
                    held,
                    value,
                    "block {number}: word {} {}",
                    hex_address(&key.0),
                    hex_word(&key.1)
                );
            }
        }
        out.push(Replayed { stage: stage.clone(), stream });
    }
    (idx, out)
}

/// The deploy blocks: `store_deployments` records exactly the registered contracts, with the
/// role the registry gives their runtime code, and ignores the creator's other contracts.
#[test]
fn e2e_deployments_are_recorded_from_the_creators_blocks() {
    let (idx, stages) = replay();
    let mut expected: HashMap<String, Role> = HashMap::new();
    for r in &stages {
        if r.stage["kind"] != "deploy" && r.stage["block"].as_u64() != Some(CREATION_BLOCK) {
            continue;
        }
        for (address, c) in r.stage["codes"].as_object().unwrap() {
            let codehash = word_of(&c["codehash"]);
            match idx.cfg.deployments.get(&codehash) {
                Some(role) => {
                    expected.insert(address.clone(), *role);
                }
                None => assert_eq!(
                    address, "0x1da990c8b0bf15d1f441782f01351acfee49bbaa",
                    "only the allowlist is an unregistered deployment in the fixture"
                ),
            }
        }
    }
    assert_eq!(expected.len(), 9, "{expected:?}");
    let recorded: HashMap<String, Role> = idx
        .deployments
        .iter()
        .map(|(a, (r, _))| (hex_address(a), *r))
        .collect();
    assert_eq!(recorded, expected);
    assert_eq!(recorded[POOL], Role::Pool);
    assert_eq!(recorded["0x19a9b39e6710aad109c829294b0841f0851c6bb4"], Role::Router);
    assert_eq!(recorded["0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f"], Role::Factory);
    // The pool's contracts were all created inside the range, so its words are tracked from
    // their first write: the hook's configuration words the creation snapshot carries were
    // written by the hook's own deployment block.
    let hook = parse_address("0x65cbd227cbc61248ae77a5fc813a29c54c092134").unwrap();
    let hook_stage = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_154_986))
        .unwrap();
    let hook_writes = hook_stage.stage["writes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| address_of(&w["address"]) == hook)
        .count();
    assert!(hook_writes >= 20, "{hook_writes} hook words written at its deployment");
    assert!(idx
        .words
        .keys()
        .any(|(a, k)| *a == hook && *k == keys::slot(0)));
}

/// The creation block emits the two components the range test expects, with the same ids,
/// tokens, static attributes and creation transaction, and their snapshot rows.
#[test]
fn e2e_creation_matches_the_range_test_expectations() {
    let (idx, stages) = replay();
    let creation = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(CREATION_BLOCK))
        .unwrap();
    let block = creation.stream.last().unwrap();
    assert_eq!(block.number, CREATION_BLOCK);
    assert_eq!(block.new.len(), 2);
    assert!(block.deltas.is_empty(), "nothing streams for a component in its creation block");
    let tests = expected_components_of_yaml();
    assert_eq!(tests.len(), 2);
    for test in &tests {
        assert_eq!(test.expected.len(), 2, "{}", test.name);
        for want in &test.expected {
            let held = block
                .new
                .get(&want.id)
                .unwrap_or_else(|| panic!("{}: {} not created", test.name, want.id));
            let c = held.component.as_ref().unwrap();
            assert_eq!(
                c.tokens
                    .iter()
                    .map(|t| hex(t))
                    .collect::<Vec<_>>(),
                want.tokens,
                "{}: tokens of {}",
                test.name,
                want.id
            );
            let statics: BTreeMap<String, String> = c
                .static_att
                .iter()
                .map(|a| (a.name.clone(), hex(&a.value)))
                .collect();
            assert_eq!(statics, want.static_attributes, "{}: statics of {}", test.name, want.id);
            assert_eq!(hex(&held.creation_tx), want.creation_tx, "{}", test.name);
            assert_eq!(held.creation_tx, hex_bytes(&Value::String(CREATION_TX.into())));
            assert_eq!(c.protocol_type.as_ref().unwrap().name, "flamm_pool");
            assert!(c.contracts.is_empty());
        }
    }
    // The snapshot rows: every attribute of the schema snapshot the creation can know, both
    // components identical, balances = the seeded physical inventory and no loan asset.
    let [swap, lever] = idx.pools[0].component_ids();
    assert_eq!(block.new[&swap].rows, block.new[&lever].rows);
    let rows = &block.new[&swap].rows;
    assert!(rows.len() > 120, "{} rows", rows.len());
    assert_eq!(rows["feed:asset:kind"], b"ocr2");
    assert_eq!(rows["feed:mo0:kind"], b"dual");
    assert_eq!(
        rows.keys()
            .filter(|k| k.starts_with("feed:mo0:tx:"))
            .count(),
        21
    );
    let physical = &rows[&format!("pool:{}", hex_word(&keys::add(&keys::FLAMM_NS, 12)))];
    let pool = &idx.pools[0];
    assert_eq!(block.new[&swap].balances[&pool.pool_asset.to_vec()], *physical);
    assert_eq!(block.new[&swap].balances[&pool.loan_assets[0].to_vec()], vec![0u8; 32]);
    // Born paused (`RiskInit.startPaused`), lever paused: the range test skips both venues.
    let ns10: Word = rows[&format!("pool:{}", hex_word(&keys::add(&keys::FLAMM_NS, 10)))]
        .as_slice()
        .try_into()
        .unwrap();
    assert_eq!(keys::field(&ns10, 22, 1), 1, "paused at creation");
    assert_eq!(keys::field(&ns10, 23, 1), 1, "levPaused at creation");
}

/// Leverage was paused from the creation until the `LevPauseSet(false)` in this block, the
/// curator Safe calling `FLAMM.setLevPaused(false)`.
const LEV_UNPAUSE_BLOCK: u64 = 51_433_699;

/// The `MaxSpreadAgeSet(0)` on the `LeverageSpreadHook` in this block, the curator Safe clearing
/// `maxSpreadAge`: until it the constructor's 17500 ppm spread post had long aged out and every
/// lever-up reverted `SpreadUnavailable`; from it the same post no longer lapses and the lever-up
/// venue quotes. Both range tests stop far below this block.
const SPREAD_AGE_CLEARED_BLOCK: u64 = 51_649_706;

/// Every block after the creation: the words the block wrote reach both components as updates
/// (a FLAMM-owned word, a Morpho word, a feed round), deletions only ever name held rows
/// (checked in `replay`), and the pause bits follow the curator: paused until 51298416, lever
/// paused until 51433699, so the range test's skip flags are truthful at every stop block.
#[test]
fn e2e_deltas_follow_the_chain_and_the_pause_bits_justify_the_skips() {
    let (idx, stages) = replay();
    let [swap, lever] = idx.pools[0].component_ids();
    let ns10 = format!("pool:{}", hex_word(&keys::add(&keys::FLAMM_NS, 10)));
    let mut rows: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut paused_until = None;
    for r in &stages {
        for b in &r.stream {
            if let Some(held) = b.new.get(&swap) {
                rows = held.rows.clone();
            }
            if let Some(d) = b.deltas.get(&swap) {
                assert_eq!(Some(d), b.deltas.get(&lever), "block {}", b.number);
                for name in &d.deleted {
                    assert!(rows.remove(name).is_some(), "block {}: {name}", b.number);
                }
                for (name, value) in &d.updated {
                    rows.insert(name.clone(), value.clone());
                }
            }
            if rows.is_empty() {
                continue;
            }
            let word: Word = rows[&ns10]
                .as_slice()
                .try_into()
                .unwrap();
            let paused = keys::field(&word, 22, 1) == 1;
            assert_eq!(
                keys::field(&word, 23, 1) == 1,
                b.number < LEV_UNPAUSE_BLOCK,
                "block {}: levPaused",
                b.number
            );
            if paused {
                paused_until = Some(b.number);
            } else {
                assert!(b.number >= 51_298_416, "block {}: unpaused early", b.number);
            }
        }
    }
    assert_eq!(paused_until, Some(51_298_415), "paused through the block before activation");
    assert_eq!(rows, idx.held[&swap].rows, "the folded deltas reach the indexer's rows");
    // The activation block: exactly the two pool words the curator moved.
    let activation = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_298_416))
        .unwrap();
    let d = &activation.stream.last().unwrap().deltas[&swap];
    assert_eq!(d.updated.len(), 2, "{:?}", d.updated.keys());
    assert!(d.deleted.is_empty());
    assert!(d.balances.is_empty());
    // The withdrawal block (`redeemToAsset` through the periphery): the pool's share supply and
    // inventory words, the Router's managed collateral, the venue's Morpho position and market
    // words with the IRM rate the accrual adapted, and the pool asset inventory; the hook's book
    // is not touched and the loan asset's liquid balance did not move.
    let withdrawal = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_348_093))
        .unwrap();
    let d = &withdrawal.stream.last().unwrap().deltas[&swap];
    let by_prefix = |d: &Delta, prefix: &str| {
        d.updated
            .keys()
            .filter(|k| k.starts_with(prefix))
            .count()
    };
    assert_eq!(d.updated.len(), 9, "{:?}", d.updated.keys());
    assert_eq!(by_prefix(d, "pool:"), 3);
    assert_eq!(by_prefix(d, "router:"), 1);
    assert_eq!(by_prefix(d, "mm:0:market:"), 3);
    assert_eq!(by_prefix(d, "mm:0:position:"), 1);
    assert_eq!(by_prefix(d, "irm:0:"), 1);
    assert!(d.deleted.is_empty());
    assert_eq!(d.balances.len(), 1);
    assert!(d
        .balances
        .contains_key(idx.pools[0].pool_asset.as_slice()));
    // A keeper recenter: the twelve words of the hook's book (`EverlongHook.recenter`), nothing of
    // the pool and no inventory.
    let recenter = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_384_803))
        .unwrap();
    let d = &recenter.stream.last().unwrap().deltas[&swap];
    assert_eq!(d.updated.len(), 12, "{:?}", d.updated.keys());
    assert_eq!(by_prefix(d, "hook:"), 12);
    assert!(d.deleted.is_empty());
    assert!(d.balances.is_empty());
    // The unpause: exactly the pool's flags word, lever unpaused, still activated.
    let unpause = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(LEV_UNPAUSE_BLOCK))
        .unwrap();
    let d = &unpause.stream.last().unwrap().deltas[&swap];
    assert_eq!(d.updated.keys().collect::<Vec<_>>(), vec![&ns10]);
    let word: Word = d.updated[&ns10]
        .as_slice()
        .try_into()
        .unwrap();
    assert_eq!(keys::field(&word, 22, 1), 0, "paused");
    assert_eq!(keys::field(&word, 23, 1), 0, "levPaused");
    assert!(d.deleted.is_empty());
    assert!(d.balances.is_empty());
    // The first swap block: the twelve words the fill moved and the pool asset inventory.
    let swap_block = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_302_916))
        .unwrap();
    let d = &swap_block.stream.last().unwrap().deltas[&swap];
    assert_eq!(d.updated.len(), 12, "{:?}", d.updated.keys());
    assert!(d
        .updated
        .contains_key("mm:0:position:1"));
    assert!(d
        .updated
        .contains_key("irm:0:rate_at_target"));
    assert_eq!(d.balances.len(), 1);
    assert_eq!(
        d.balances[&idx.pools[0].pool_asset.to_vec()],
        keys::word_from_u64(0x32d4a + 0x666d).to_vec()
    );
    // A Chainlink round block: the feed's round attributes move and nothing else of the feed.
    let round = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_433_135))
        .unwrap();
    let d = &round.stream.last().unwrap().deltas[&swap];
    for name in
        ["feed:asset:round", "feed:asset:answer", "feed:asset:started_at", "feed:asset:updated_at"]
    {
        assert!(d.updated.contains_key(name), "{name}");
    }
    let dual = stages
        .iter()
        .find(|r| r.stage["block"].as_u64() == Some(51_433_218))
        .unwrap();
    let d = &dual.stream.last().unwrap().deltas[&swap];
    assert!(d.updated.contains_key("feed:mo0:round"));
    assert!(d
        .updated
        .keys()
        .any(|k| k.starts_with("feed:mo0:tx:")));
    assert!(
        d.deleted
            .iter()
            .any(|k| k.starts_with("feed:mo0:tx:")),
        "the round that left the window"
    );
}

/// The schema snapshot of the swap component at 51302915 (`snapshot_51302915.json`, every value
/// verified against the contracts' views) is what the fold holds there: the same attribute
/// names, the same values, the same balances.
#[test]
fn e2e_fold_at_51302915_is_the_schema_snapshot() {
    let (idx, stages) = replay();
    let [swap, _] = idx.pools[0].component_ids();
    let snap = testdata::fixture("snapshot");
    let mut rows: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut balances: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for r in &stages {
        for b in &r.stream {
            if let Some(held) = b.new.get(&swap) {
                rows = held.rows.clone();
                balances = held.balances.clone();
            }
            if let Some(d) = b.deltas.get(&swap) {
                for name in &d.deleted {
                    rows.remove(name);
                }
                rows.extend(d.updated.clone());
                balances.extend(d.balances.clone());
            }
        }
        if r.stage["block"].as_u64() == Some(51_302_915) {
            break;
        }
    }
    let want: BTreeMap<String, Vec<u8>> = snap["attributes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), hex_bytes(v)))
        .collect();
    // The fold carries the schema's attributes plus `feed:<f>:kind` (the package's addition) and
    // any FLAMM-owned word outside the schema's list that was written (forwarded whole ranges).
    let owned = |name: &str| {
        ["pool:", "hook:", "spread:", "router:", "account:", "pricefeed:", "factory:"]
            .iter()
            .any(|p| name.starts_with(p))
    };
    for (name, value) in &want {
        let Some(got) = rows.get(name) else {
            // A FLAMM-owned word the contract never wrote is absent and reads as zero.
            assert!(owned(name) && keys::is_zero(value), "missing {name}");
            continue;
        };
        let got = if got.len() > value.len() && keys::is_zero(&got[..got.len() - value.len()]) {
            &got[got.len() - value.len()..]
        } else {
            &got[..]
        };
        assert_eq!(got, value.as_slice(), "{name}");
    }
    let extra: Vec<&String> = rows
        .keys()
        .filter(|k| !want.contains_key(*k))
        .collect();
    for name in &extra {
        assert!(
            name.ends_with(":kind") ||
                name.starts_with("pool:") ||
                name.starts_with("hook:") ||
                name.starts_with("router:"),
            "unexpected extra attribute {name}"
        );
    }
    // Every schema word that reads as zero and was never written is absent: the decoder reads
    // absent FLAMM-owned words as zero.
    for (token, balance) in snap["balances"].as_object().unwrap() {
        let got = &balances[&hex_bytes(&Value::String(token.clone()))];
        let want = hex_u64(balance);
        assert_eq!(
            keys::field(&keys::parse_word(&hex::encode(got)).unwrap(), 0, 32),
            want as u128,
            "{token}"
        );
    }
}

/// The stream fixture the simulation replays is this package's output: the fold of every stage
/// block, serialized, equals the committed `e2e_stream.json.gz`. Set `FLAMM_E2E_WRITE=<path>` to
/// rewrite it (the ProtocolSim tests then pin its digest).
#[test]
fn e2e_stream_fixture_is_the_package_output() {
    let (_, stages) = replay();
    let blocks: Vec<Value> = stages
        .iter()
        .flat_map(|r| {
            r.stream
                .iter()
                .map(StreamBlock::to_json)
        })
        .collect();
    let produced = json!({
        "note": "The base-flamm package's output over real Base blocks (protocols/substreams/base-flamm/testdata/e2e_blocks.json.gz), folded as tycho-storage holds it and as tycho-client delivers it: per block, the components created in it with their snapshot (component, attribute rows, balances) and the merged deltas of the components that existed before it. Produced by base-flamm's e2e_stream_fixture_is_the_package_output test.",
        "blocks": blocks,
    });
    if let Ok(path) = std::env::var("FLAMM_E2E_WRITE") {
        let raw = serde_json::to_vec(&produced).unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        std::fs::write(&path, enc.finish().unwrap()).unwrap();
        eprintln!("wrote {path} ({} bytes raw)", raw.len());
    }
    let committed = committed_stream();
    let committed_blocks = committed["blocks"].as_array().unwrap();
    assert_eq!(committed_blocks.len(), blocks.len(), "block count");
    for (got, want) in blocks.iter().zip(committed_blocks) {
        assert_eq!(got, want, "block {}", got["number"]);
    }
}

/// One expected component of `integration_test.tycho.yaml`.
#[derive(Debug)]
struct ExpectedComponent {
    id: String,
    tokens: Vec<String>,
    static_attributes: BTreeMap<String, String>,
    creation_tx: String,
    skip_simulation: bool,
    skip_execution: bool,
}

#[derive(Debug)]
struct ExpectedTest {
    name: String,
    start_block: u64,
    stop_block: u64,
    expected: Vec<ExpectedComponent>,
}

/// A minimal reading of the range test's yaml: the `tests` list with each test's name, block
/// range and expected components (id, tokens, static attributes, creation tx, skip flags).
/// Comments are stripped, values unquoted; the layout is the one the file uses.
fn expected_components_of_yaml() -> Vec<ExpectedTest> {
    let text = include_str!("../integration_test.tycho.yaml");
    let mut tests: Vec<ExpectedTest> = Vec::new();
    let mut section = "";
    for raw in text.lines() {
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        if indent == 0 && body == "tests:" {
            section = "tests";
            continue;
        }
        if indent == 0 {
            section = "";
            continue;
        }
        if section != "tests" {
            continue;
        }
        if indent == 2 && body.starts_with("- name:") {
            tests.push(ExpectedTest {
                name: value_of(body.trim_start_matches("- name:")),
                start_block: 0,
                stop_block: 0,
                expected: Vec::new(),
            });
            continue;
        }
        let test = tests
            .last_mut()
            .expect("a test before its fields");
        if indent == 4 {
            if let Some(v) = body.strip_prefix("start_block:") {
                test.start_block = value_of(v).parse().unwrap();
            } else if let Some(v) = body.strip_prefix("stop_block:") {
                test.stop_block = value_of(v).parse().unwrap();
            }
            continue;
        }
        if indent == 6 && body.starts_with("- id:") {
            test.expected.push(ExpectedComponent {
                id: value_of(body.trim_start_matches("- id:")),
                tokens: Vec::new(),
                static_attributes: BTreeMap::new(),
                creation_tx: String::new(),
                skip_simulation: false,
                skip_execution: false,
            });
            continue;
        }
        let Some(component) = test.expected.last_mut() else { continue };
        if indent == 8 {
            if let Some(v) = body.strip_prefix("creation_tx:") {
                component.creation_tx = value_of(v);
            } else if let Some(v) = body.strip_prefix("skip_simulation:") {
                component.skip_simulation = value_of(v) == "true";
            } else if let Some(v) = body.strip_prefix("skip_execution:") {
                component.skip_execution = value_of(v) == "true";
            }
            continue;
        }
        if indent == 10 {
            if let Some(v) = body.strip_prefix("- ") {
                component.tokens.push(value_of(v));
            } else if let Some((k, v)) = body.split_once(':') {
                component
                    .static_attributes
                    .insert(k.trim().to_string(), value_of(v));
            }
        }
    }
    tests
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

fn value_of(v: &str) -> String {
    v.trim().trim_matches('"').to_string()
}

/// The yaml's skip flags are what the chain justifies at each stop block: the swap venue is
/// paused through the first test's range (both skips true) and quotes in the second (simulation
/// and execution on: the harness runs the quoted sizes through the `FLAMMExecutor` it holds under
/// `flamm`); the lever-up venue is paused at both stop blocks (simulation off, and execution with
/// it; leverage stays paused until 51433699, and through the unpause `previewLever` still refused
/// every size, `SpreadUnavailable`, the keeper never having re-posted a spread -- until the
/// curator cleared `maxSpreadAge` at 51649706, far past both stop blocks, from where the venue
/// quotes), and both tests span the creation block.
#[test]
fn e2e_yaml_skip_flags_are_truthful() {
    let tests = expected_components_of_yaml();
    let (_, stages) = replay();
    let by_block: HashMap<u64, &Replayed> = stages
        .iter()
        .map(|r| (r.stage["block"].as_u64().unwrap(), r))
        .collect();
    let lever = keys::hex_word(&{
        let mut id = [0u8; 32];
        id[..20].copy_from_slice(&parse_address(POOL).unwrap());
        id[31] = 1;
        id
    });
    for test in &tests {
        assert_eq!(test.start_block, 51_154_966, "{}", test.name);
        assert!(test.start_block < CREATION_BLOCK && test.stop_block > CREATION_BLOCK);
        let stop = by_block
            .get(&test.stop_block)
            .unwrap_or_else(|| {
                panic!("{}: stop block {} is not a fixture stage", test.name, test.stop_block)
            });
        let grids = &stop.stage["grids"];
        let quotable = grids["swap_sell"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["ok"].as_bool() == Some(true));
        assert!(test.stop_block < LEV_UNPAUSE_BLOCK);
        assert!(test.stop_block < SPREAD_AGE_CLEARED_BLOCK);
        for c in &test.expected {
            assert_eq!(
                c.skip_execution, c.skip_simulation,
                "{}: execution is skipped exactly where simulation is",
                test.name
            );
            if c.id == lever {
                assert!(c.skip_simulation, "{}: lever-up is paused on chain", test.name);
                assert!(grids["lever_up"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["ok"].as_bool() == Some(false)));
            } else {
                assert_eq!(c.id, POOL);
                assert_eq!(
                    !c.skip_simulation, quotable,
                    "{}: skip_simulation vs previewSwap at {}",
                    test.name, test.stop_block
                );
            }
        }
    }
}
