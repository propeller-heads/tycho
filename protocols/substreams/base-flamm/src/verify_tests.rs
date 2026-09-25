// Copyright (c) 2026 Everlong Labs Limited
//! The curator's activation block replayed from real `eth_getStorageAt` diffs, synthetic blocks
//! that probe the ordering paths (a word written twice in one transaction, a tracked write before
//! the pool's creation transaction, rotations of the Morpho oracle's proxy with and without a
//! round of the same block, a secondary-only `DualAggregator` transmission, a Morpho accrual that
//! moves nothing the pool recognizes, the attribute set of a `DualAggregator` round), and
//! multi-block histories replayed against the indexer's storage rule: a `Deletion` must name a
//! row the indexer holds, and the rows of a feed are its state derived from the words store.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use substreams_ethereum::pb::eth::v2::{Log, StorageChange};
use tycho_substreams::prelude::{BlockChanges, BlockTransactionProtocolComponents, ChangeType};

use crate::{
    config::Config,
    flamm::{
        feed_events, feeds,
        feeds::FeedKind,
        keys::{self, field, parse_address, Address, Word},
        statics, PoolConfig, Role,
    },
    modules::{components_in_block, protocol_changes, tracked_writes},
    testdata::{self, fixture, hex_bytes, hex_u64, synthetic_ring, words_map},
};

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

fn live_pool() -> PoolConfig {
    let snap = fixture("snapshot");
    let statics: Vec<(String, Vec<u8>)> = snap["component"]["static_attributes"]
        .as_object()
        .expect("statics")
        .iter()
        .map(|(k, v)| (k.clone(), hex_bytes(v)))
        .collect();
    let component = tycho_substreams::prelude::ProtocolComponent::new(
        snap["component"]["id"]
            .as_str()
            .unwrap(),
    )
    .with_attributes(&statics);
    statics::pool_config_from_component(&component).expect("pool config")
}

fn attrs_of(
    changes: &tycho_substreams::prelude::TransactionChanges,
    component_id: &str,
) -> BTreeMap<String, (Vec<u8>, i32)> {
    changes
        .entity_changes
        .iter()
        .find(|e| e.component_id == component_id)
        .map(|e| {
            e.attributes
                .iter()
                .map(|a| (a.name.clone(), (a.value.clone(), a.change)))
                .collect()
        })
        .unwrap_or_default()
}

/// The curator's activation (block 51298416, tx 0x4af4828c…eafd, index 75): real before/after
/// `eth_getStorageAt` over every tracked word (plus the proxies' rotation/access words and the
/// aggregators' hot words) shows exactly two pool words moved: the pause byte of `FLAMM_NS+10`
/// and `features` (`FLAMM_NS+23`, 0x3b -> 0x3f). The package must forward exactly those two as
/// `pool:<slot>` updates on both components and emit no balance change (no inventory word moved).
#[test]
fn verify_activation_block_51298416_forwards_exactly_the_two_pool_words() {
    let f = fixture("activation");
    let cfg = config();
    let pool = live_pool();
    let block = testdata::fixture_block(&f, vec![]);
    let before = words_map(&f["words_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, first_word);
    assert_eq!(changes.block.as_ref().unwrap().number, 51298416);
    assert_eq!(changes.block.as_ref().unwrap().ts, 0x6aa7ddc3);
    assert_eq!(changes.changes.len(), 1);
    let tx = &changes.changes[0];
    assert_eq!(tx.tx.as_ref().unwrap().index, 0x4b);
    assert!(tx.component_changes.is_empty());
    assert!(tx.balance_changes.is_empty(), "no inventory word moved at activation");
    let [swap_id, lever_id] = pool.component_ids();
    let got = attrs_of(tx, &swap_id);
    assert_eq!(got, attrs_of(tx, &lever_id));
    let mut want = BTreeMap::new();
    for d in f["storage_diffs"].as_array().unwrap() {
        let name = format!("pool:{}", d["slot"].as_str().unwrap());
        want.insert(name, (hex_bytes(&d["new"]), i32::from(ChangeType::Update)));
    }
    assert_eq!(want.len(), 2);
    assert_eq!(got, want);
    // Decoded: `paused` (byte 22 of FLAMM_NS+10, FLAMMStore.sol:262) 1 -> 0, `initialized` and
    // `bootstrapped` (bytes 20, 21) still 1, `levPaused` (byte 23) still 1; features 0x3b -> 0x3f.
    let ns10 = &got[&format!("pool:{}", keys::hex_word(&keys::add(&keys::FLAMM_NS, 10)))].0;
    let ns10: Word = ns10.as_slice().try_into().unwrap();
    assert_eq!(field(&ns10, 20, 1), 1);
    assert_eq!(field(&ns10, 21, 1), 1);
    assert_eq!(field(&ns10, 22, 1), 0, "paused cleared");
    assert_eq!(field(&ns10, 23, 1), 1, "levPaused still set");
    let old10: Word = hex_bytes(&f["storage_diffs"][0]["old"])
        .as_slice()
        .try_into()
        .unwrap();
    assert_eq!(field(&old10, 22, 1), 1, "was paused before activation");
    let features = &got[&format!("pool:{}", keys::hex_word(&keys::add(&keys::FLAMM_NS, 23)))].0;
    assert_eq!(features[31], 0x3f);
}

fn storage_change(address: &Address, key: &Word, value: &Word, ordinal: u64) -> StorageChange {
    StorageChange {
        address: address.to_vec(),
        key: key.to_vec(),
        old_value: vec![],
        new_value: value.to_vec(),
        ordinal,
    }
}

fn tx_with(
    index: u32,
    to: &Address,
    logs: Vec<Log>,
    writes: Vec<StorageChange>,
) -> substreams_ethereum::pb::eth::v2::TransactionTrace {
    testdata::transaction(testdata::TxSpec {
        index,
        hash: vec![index as u8; 32],
        from: vec![2; 20],
        to: to.to_vec(),
        input: vec![],
        logs,
        storage_changes: writes,
        code_changes: vec![],
        create: false,
    })
}

/// Two writes of one word inside one transaction: the attribute carries the last value.
#[test]
fn verify_same_word_written_twice_in_one_tx_keeps_the_last_value() {
    let cfg = config();
    let pool = live_pool();
    let swap = fixture("swap");
    let before = words_map(&swap["words_before"]);
    let key = keys::add(&keys::FLAMM_NS, 12);
    let tx = tx_with(
        7,
        &pool.pool,
        vec![],
        vec![
            storage_change(&pool.pool, &key, &keys::word_from_u64(1), 10),
            storage_change(&pool.pool, &key, &keys::word_from_u64(2), 11),
        ],
    );
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        before.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[&format!("pool:{}", keys::hex_word(&key))],
        (keys::word_from_u64(2).to_vec(), i32::from(ChangeType::Update))
    );
    // and the inventory is valued at the last write too
    let balance = changes.changes[0]
        .balance_changes
        .iter()
        .find(|b| {
            b.component_id == pool.component_ids()[0].as_bytes() && b.token == pool.pool_asset
        })
        .expect("pool asset balance");
    assert_eq!(balance.balance, keys::word_from_u64(2).to_vec());
}

/// A block where a tracked word moves in a transaction *before* the pool's creation transaction:
/// the pool does not exist yet at that transaction, so no entity change may be attributed to its
/// components there (the creation snapshot at the later transaction already carries the value).
#[test]
fn verify_no_state_is_emitted_for_a_pool_before_its_creation_tx() {
    let creation = fixture("creation");
    let cfg = config();
    let deployments: HashMap<Address, (crate::flamm::Role, Word)> = creation["codehashes"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(a, h)| {
            let codehash = keys::parse_word(h.as_str().unwrap()).unwrap();
            let role = *cfg.deployments.get(&codehash)?;
            Some((parse_address(a).unwrap(), (role, codehash)))
        })
        .collect();
    let deployment = |a: &Address| deployments.get(a).copied();
    let before = words_map(&creation["store_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    let mut block = testdata::fixture_block(&creation, vec![]);
    let creation_index = block.transaction_traces[0].index;
    assert_eq!(creation_index, 159);
    // tx 3: a Morpho accrual writes the market's totals word (23% of Base blocks do)
    let morpho = pool_morpho(&cfg);
    let market0 = keys::morpho_market_keys(&market_id())[0];
    let early = tx_with(
        3,
        &morpho,
        vec![],
        vec![storage_change(&morpho, &market0, &keys::word_from_u64(77), 1)],
    );
    block
        .transaction_traces
        .insert(0, early);
    let components = components_in_block(&block, &cfg, &deployment, first_word);
    assert_eq!(components.tx_components.len(), 1);
    let changes = protocol_changes(&block, &cfg, vec![], &components, first_word);
    let ids: Vec<String> = components.tx_components[0]
        .components
        .iter()
        .map(|c| c.id.clone())
        .collect();
    for tx in &changes.changes {
        let index = tx.tx.as_ref().unwrap().index;
        if index < creation_index as u64 {
            for id in &ids {
                assert!(
                    !tx.entity_changes
                        .iter()
                        .any(|e| &e.component_id == id),
                    "tx {index} emits state for {id}, created at tx {creation_index}"
                );
            }
            assert!(tx.balance_changes.is_empty());
        }
    }
    // the block's only change is the creation transaction, whose snapshot values the totals word
    // after the early write
    assert_eq!(changes.changes.len(), 1);
    assert_eq!(
        changes.changes[0]
            .tx
            .as_ref()
            .unwrap()
            .index,
        creation_index as u64
    );
    let got = attrs_of(&changes.changes[0], &ids[0]);
    assert_eq!(
        got["mm:0:market:0"],
        (keys::word_from_u64(77).to_vec(), i32::from(ChangeType::Creation))
    );
}

fn market_id() -> Word {
    keys::parse_word("0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836").unwrap()
}

fn pool_morpho(_cfg: &Config) -> Address {
    parse_address("0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb").unwrap()
}

/// A rotation of the Morpho oracle's proxy (`mo0`) to an aggregator the manifest does not list:
/// the previous DualAggregator's state (its rounds, cutoff, kind and the ring `feed:mo0:tx:<r>`,
/// 21 seeded rounds) is deleted, the window being the one its hot words gave before the
/// transaction (schema 2.6: a proxy rotation deletes the previous aggregator's rounds).
#[test]
fn verify_mo0_rotation_deletes_the_previous_ring() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let proxy = pool.feed("mo0").unwrap().proxy;
    let new_agg = [0x99u8; 20];
    let mut phase = [0u8; 32];
    phase[30..].copy_from_slice(&4u16.to_be_bytes());
    phase[10..30].copy_from_slice(&new_agg);
    let tx = tx_with(2, &proxy, vec![], vec![storage_change(&proxy, &keys::slot(2), &phase, 1)]);
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got["feed:mo0:aggregator"].0, new_agg.to_vec());
    assert_eq!(got["feed:mo0:round"].1, i32::from(ChangeType::Deletion));
    let ring = feeds::dual_ring(0xbcb, 0xbc9);
    assert_eq!(ring.len(), 21);
    let deleted: Vec<_> = ring
        .iter()
        .filter(|r| {
            got.get(&format!("feed:mo0:tx:{r}"))
                .is_some_and(|(_, c)| *c == i32::from(ChangeType::Deletion))
        })
        .collect();
    assert_eq!(deleted.len(), 21, "ring entries deleted on rotation: {deleted:?}");
    assert_eq!(
        got.keys()
            .filter(|k| k.starts_with("feed:mo0:tx:"))
            .count(),
        21,
        "nothing outside the window"
    );
}

/// A round of the previous DualAggregator and the rotation in one transaction, the round first:
/// the indexer sees the transaction's end state, so the round never surfaces (neither its ring
/// entry nor its round id is emitted, and nothing is deleted that was never a row) and what is
/// deleted is exactly the state the feed had before the transaction, the seeded 21-round ring.
#[test]
fn verify_round_then_rotation_in_one_tx_deletes_the_state_before_the_tx() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let proxy = pool.feed("mo0").unwrap().proxy;
    let agg = parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap();
    let latest = 0xbcbu32;
    let next = latest + 1;
    let block_ts = 1789600000u64;
    let mut logs = dual_round_logs(&agg, next, block_ts - 3, block_ts, 10);
    logs.remove(0); // a primary round: no SecondaryRoundIdUpdated
    let mut hotvars = seeds[&(agg, keys::slot(13))];
    hotvars[32 - 6 - 4..32 - 6].copy_from_slice(&next.to_be_bytes());
    let transmission = feeds::pack_transmission(
        &keys::word_from_u64(0x6fc00000000),
        block_ts as u32 - 3,
        block_ts as u32,
    );
    let new_agg = [0x99u8; 20];
    let mut phase = [0u8; 32];
    phase[30..].copy_from_slice(&4u16.to_be_bytes());
    phase[10..30].copy_from_slice(&new_agg);
    let tx = tx_with(
        2,
        &proxy,
        logs,
        vec![
            // the transmit's writes (`DualAggregator.sol:931-944`), then the rotation
            storage_change(
                &agg,
                &FeedKind::Dual
                    .transmission(next)
                    .unwrap(),
                &transmission,
                5,
            ),
            storage_change(&agg, &keys::slot(13), &hotvars, 6),
            storage_change(&proxy, &keys::slot(2), &phase, 20),
        ],
    );
    let block = testdata::block(51400000, block_ts, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got["feed:mo0:aggregator"].0, new_agg.to_vec());
    assert_eq!(got["feed:mo0:round"].1, i32::from(ChangeType::Deletion));
    assert!(!got.contains_key(&format!("feed:mo0:tx:{next}")), "the round never became a row");
    let deleted: Vec<u32> = (0..=next)
        .filter(|r| {
            got.get(&format!("feed:mo0:tx:{r}"))
                .is_some_and(|(_, c)| *c == i32::from(ChangeType::Deletion))
        })
        .collect();
    assert_eq!(deleted, feeds::dual_ring(latest, 0xbc9));
    assert!(
        got.iter()
            .all(|(k, (_, c))| !k.starts_with("feed:mo0:tx:") ||
                *c == i32::from(ChangeType::Deletion))
    );
}

/// The rotation, then a round of the *new* aggregator (unknown to the manifest) in the same
/// transaction: nothing of an unlisted aggregator is tracked, so its round is not decoded and the
/// feed is left with `aggregator` and `phase` (and the proxy's access controller), failing closed
/// until a package update lists it.
#[test]
fn verify_rotation_to_an_unlisted_aggregator_leaves_only_aggregator_and_phase() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let proxy = pool.feed("mo0").unwrap().proxy;
    let new_agg = [0x99u8; 20];
    let block_ts = 1789600000u64;
    let mut logs = dual_round_logs(&new_agg, 7, block_ts - 3, block_ts, 30);
    logs.remove(0);
    let mut phase = [0u8; 32];
    phase[30..].copy_from_slice(&4u16.to_be_bytes());
    phase[10..30].copy_from_slice(&new_agg);
    let mut hotvars = [0u8; 32];
    hotvars[32 - 6 - 4..32 - 6].copy_from_slice(&7u32.to_be_bytes());
    let tx = tx_with(
        2,
        &proxy,
        logs,
        vec![
            storage_change(&proxy, &keys::slot(2), &phase, 20),
            storage_change(&new_agg, &FeedKind::Dual.transmission(7).unwrap(), &[7u8; 32], 25),
            storage_change(&new_agg, &keys::slot(13), &hotvars, 26),
        ],
    );
    let block = testdata::block(51400000, block_ts, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got["feed:mo0:aggregator"].0, new_agg.to_vec());
    assert_eq!(got["feed:mo0:phase"].0, keys::word_from_u64(4).to_vec());
    assert_eq!(got["feed:mo0:kind"].1, i32::from(ChangeType::Deletion), "kind unknown");
    for name in ["round", "secondary_round", "cutoff"] {
        assert_eq!(got[&format!("feed:mo0:{name}")].1, i32::from(ChangeType::Deletion), "{name}");
    }
    assert!(!got.contains_key("feed:mo0:tx:7"));
    assert_eq!(
        feeds::dual_ring(0xbcb, 0xbc9)
            .iter()
            .filter(|r| got[&format!("feed:mo0:tx:{r}")].1 == i32::from(ChangeType::Deletion))
            .count(),
        21
    );
    let updated: Vec<&String> = got
        .iter()
        .filter(|(_, (_, c))| *c != i32::from(ChangeType::Deletion))
        .map(|(k, _)| k)
        .collect();
    assert_eq!(updated, vec!["feed:mo0:aggregator", "feed:mo0:phase"]);
}

/// The logs of one `DualAggregator` round `round` (secondary-first order), from ordinal
/// `ordinal`.
fn dual_round_logs(
    agg: &Address,
    round: u32,
    observations_ts: u64,
    block_ts: u64,
    ordinal: u64,
) -> Vec<Log> {
    let mut round_topic = [0u8; 32];
    round_topic[28..].copy_from_slice(&round.to_be_bytes());
    let answer = keys::word_from_u64(0x6fc00000000);
    let mut nt_data = Vec::new();
    nt_data.extend_from_slice(&answer);
    nt_data.extend_from_slice(&[0u8; 32]); // transmitter
    nt_data.extend_from_slice(&keys::word_from_u64(observations_ts));
    nt_data.extend_from_slice(&[0u8; 32 * 6]); // the dynamic tail is not decoded
    vec![
        Log {
            address: agg.to_vec(),
            topics: vec![feed_events::SECONDARY_ROUND_TOPIC.to_vec(), round_topic.to_vec()],
            data: vec![],
            index: 0,
            block_index: 0,
            ordinal,
        },
        Log {
            address: agg.to_vec(),
            topics: vec![feed_events::NEW_TRANSMISSION_TOPIC.to_vec(), round_topic.to_vec()],
            data: nt_data,
            index: 1,
            block_index: 1,
            ordinal: ordinal + 1,
        },
        Log {
            address: agg.to_vec(),
            topics: vec![
                feed_events::NEW_ROUND_TOPIC.to_vec(),
                round_topic.to_vec(),
                [0u8; 32].to_vec(),
            ],
            data: keys::word_from_u64(observations_ts).to_vec(),
            index: 2,
            block_index: 2,
            ordinal: ordinal + 2,
        },
        Log {
            address: agg.to_vec(),
            topics: vec![
                feed_events::ANSWER_UPDATED_TOPIC.to_vec(),
                answer.to_vec(),
                round_topic.to_vec(),
            ],
            data: keys::word_from_u64(block_ts).to_vec(),
            index: 3,
            block_index: 3,
            ordinal: ordinal + 3,
        },
    ]
}

/// The same rotation, with a secondary-round reveal of the old aggregator earlier in the block
/// (`transmitSecondary` of an existing round, `DualAggregator.sol:753-758`): the reveal moves
/// `secondary_round` only (the new secondary round is inside the window), and the rotation then
/// deletes the whole ring as the reveal left it.
#[test]
fn verify_mo0_rotation_after_a_round_in_the_same_block_deletes_the_ring() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let proxy = pool.feed("mo0").unwrap().proxy;
    let agg = parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap();
    let new_agg = [0x99u8; 20];
    let mut phase = [0u8; 32];
    phase[30..].copy_from_slice(&4u16.to_be_bytes());
    phase[10..30].copy_from_slice(&new_agg);
    // tx 1: a secondary-round update on the old aggregator (its `HotVars` write and event);
    // tx 2: the rotation.
    let mut topic1 = [0u8; 32];
    topic1[28..].copy_from_slice(&(0xbcau32).to_be_bytes());
    let secondary_log = Log {
        address: agg.to_vec(),
        topics: vec![feed_events::SECONDARY_ROUND_TOPIC.to_vec(), topic1.to_vec()],
        data: vec![],
        index: 0,
        block_index: 0,
        ordinal: 5,
    };
    let mut hotvars = seeds[&(agg, keys::slot(13))];
    hotvars[32 - 10 - 4..32 - 10].copy_from_slice(&0xbcau32.to_be_bytes());
    let tx1 = tx_with(
        1,
        &agg,
        vec![secondary_log],
        vec![storage_change(&agg, &keys::slot(13), &hotvars, 4)],
    );
    let tx2 = tx_with(2, &proxy, vec![], vec![storage_change(&proxy, &keys::slot(2), &phase, 20)]);
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx1, tx2]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    assert_eq!(changes.changes.len(), 2);
    let reveal = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(
        reveal,
        BTreeMap::from([(
            "feed:mo0:secondary_round".to_string(),
            (keys::word_from_u64(0xbca).to_vec(), i32::from(ChangeType::Update))
        )])
    );
    let got = attrs_of(&changes.changes[1], &pool.component_ids()[0]);
    let ring = feeds::dual_ring(0xbcb, 0xbca);
    let deleted = ring
        .iter()
        .filter(|r| {
            got.get(&format!("feed:mo0:tx:{r}"))
                .is_some_and(|(_, c)| *c == i32::from(ChangeType::Deletion))
        })
        .count();
    assert_eq!(deleted, ring.len());
    assert_eq!(
        got.keys()
            .filter(|k| k.starts_with("feed:mo0:tx:"))
            .count(),
        ring.len()
    );
}

/// A secondary-only transmission (`DualAggregator._report` with `isSecondary`,
/// `DualAggregator.sol:931-944`): a new round `L+1` whose transmission is written, and `HotVars`
/// with both `latestAggregatorRoundId` and `latestSecondaryRoundId` at `L+1`. The ring after the
/// block must be `L+1-20 ..= L+1` with `L-20` evicted and the secondary round `L+1`.
#[test]
fn verify_secondary_only_transmission_keeps_the_ring_consistent() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let agg = parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap();
    let latest = 0xbcbu32;
    let next = latest + 1;
    let answer = keys::word_from_u64(0x6fc00000000);
    let observations_ts = 1789600000u64 - 3;
    let block_ts = 1789600000u64;
    let logs = dual_round_logs(&agg, next, observations_ts, block_ts, 10);
    let packed = feeds::pack_transmission(&answer, observations_ts as u32, block_ts as u32);
    let mut hotvars = seeds[&(agg, keys::slot(13))];
    hotvars[32 - 6 - 4..32 - 6].copy_from_slice(&next.to_be_bytes());
    hotvars[32 - 10 - 4..32 - 10].copy_from_slice(&next.to_be_bytes());
    let tx = tx_with(
        1,
        &agg,
        logs,
        vec![
            storage_change(
                &agg,
                &FeedKind::Dual
                    .transmission(next)
                    .unwrap(),
                &packed,
                5,
            ),
            storage_change(&agg, &keys::slot(13), &hotvars, 6),
        ],
    );
    let block = testdata::block(51400000, block_ts, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got["feed:mo0:secondary_round"].0, keys::word_from_u64(next as u64).to_vec());
    assert_eq!(got["feed:mo0:round"].0, keys::word_from_u64(next as u64).to_vec());
    assert_eq!(
        got[&format!("feed:mo0:tx:{next}")],
        (packed.to_vec(), i32::from(ChangeType::Creation))
    );
    let evicted: Vec<u32> = (0..=next)
        .filter(|r| {
            got.get(&format!("feed:mo0:tx:{r}"))
                .is_some_and(|(_, c)| *c == i32::from(ChangeType::Deletion))
        })
        .collect();
    assert_eq!(evicted, vec![latest - 20], "only the round that left the window is evicted");
    assert_eq!(got.len(), 4);
}

/// A Morpho accrual that moves only the market totals, with no managed supply, moves nothing the
/// pool recognizes: the word is forwarded, no balance is re-emitted.
#[test]
fn verify_market_totals_move_emits_no_balance() {
    let cfg = config();
    let pool = live_pool();
    let swap = fixture("swap");
    let before = words_map(&swap["words_before"]);
    let morpho = pool_morpho(&cfg);
    let market0 = keys::morpho_market_keys(&market_id())[0];
    let tx = tx_with(
        1,
        &morpho,
        vec![],
        vec![storage_change(&morpho, &market0, &keys::word_from_u64(77), 1)],
    );
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        before.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got.len(), 1);
    assert!(got.contains_key("mm:0:market:0"));
    // the pool has no managed supply at 51302915, so the inventory does not depend on the totals
    assert!(changes.changes[0]
        .balance_changes
        .is_empty());

    // With managed supply shares on the venue, the same accrual re-values the recognized supply:
    // the loan asset's balance moves, the pool asset's does not.
    let venue_head = keys::router_venue(&pool.pool, 0);
    let mut before = before.clone();
    let shares = keys::word_from_u64(5_000_000_000);
    before.insert((pool.router, keys::add(&venue_head, 5)), shares);
    let [shares_key, _] = keys::morpho_position_keys(&market_id(), &pool.venues[0].account);
    before.insert((morpho, shares_key), shares);
    // `Market { uint128 totalSupplyAssets; uint128 totalSupplyShares; }`: assets in the low half
    let mut totals = [0u8; 32];
    totals[16..].copy_from_slice(&(3_000_000_000_000u128).to_be_bytes());
    totals[..16].copy_from_slice(&(2_000_000_000_000_000u128).to_be_bytes());
    let mut moved = totals;
    moved[16..].copy_from_slice(&(3_000_100_000_000u128).to_be_bytes());
    before.insert((morpho, market0), totals);
    let tx = tx_with(1, &morpho, vec![], vec![storage_change(&morpho, &market0, &moved, 1)]);
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx]);
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        before.get(&(*a, *k)).copied()
    });
    let balances: Vec<_> = changes.changes[0]
        .balance_changes
        .iter()
        .filter(|b| b.component_id == pool.component_ids()[0].as_bytes())
        .collect();
    assert_eq!(balances.len(), 1);
    assert_eq!(balances[0].token, pool.loan_assets[0].to_vec());
    let liquid =
        crate::flamm::keys::field(&before[&(pool.pool, keys::add(&keys::pool_loan(0), 5))], 0, 32);
    let recognized = crate::flamm::balances::supply_shares_to_assets(
        ethabi::ethereum_types::U256::from(5_000_000_000u64),
        ethabi::ethereum_types::U256::from(3_000_100_000_000u64),
        ethabi::ethereum_types::U256::from(2_000_000_000_000_000u64),
    )
    .unwrap();
    assert_eq!(
        balances[0].balance,
        crate::flamm::balances::balance_bytes(
            &(ethabi::ethereum_types::U256::from(liquid) + recognized)
        )
    );
    assert_eq!(changes.changes[0].balance_changes.len(), 2, "one per component");
}

/// The attribute names a DualAggregator round adds beyond the schema's `mo0` set.
#[test]
fn verify_mo0_round_attribute_names_versus_schema() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let logs = fixture("feed_logs");
    let entry = &logs["mo0_primary"];
    let cfg = config();
    let pool = live_pool();
    let agg = parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap();
    let mut words = seeds.clone();
    let after = crate::testdata::word(&entry["hotvars_after"]);
    let (latest, secondary) = feeds::dual_hotvars(&after);
    let mut before = after;
    before[32 - 6 - 4..32 - 6].copy_from_slice(&(latest - 1).to_be_bytes());
    words.insert((agg, keys::slot(13)), before);
    synthetic_ring(&mut words, &agg, latest - 1, secondary);
    let logs: Vec<_> = entry["logs"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, l)| testdata::log(100 + i as u64, l))
        .collect();
    let transmission = crate::testdata::word(&entry["transmission_after"]);
    let tx = tx_with(
        3,
        &agg,
        logs,
        vec![
            storage_change(
                &agg,
                &FeedKind::Dual
                    .transmission(latest)
                    .unwrap(),
                &transmission,
                5,
            ),
            storage_change(&agg, &keys::slot(13), &after, 6),
        ],
    );
    let block = testdata::block(
        entry["block"].as_u64().unwrap(),
        hex_u64(&entry["timestamp"]),
        vec![4; 32],
        vec![5; 32],
        vec![tx],
    );
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        words.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    // schema 3.2 lists for mo0: aggregator, phase, access_controller, round, secondary_round,
    // cutoff, tx:<round>; a round moves `round`, its ring entry and the evicted one
    let names: Vec<&String> = got.keys().collect();
    assert_eq!(
        names,
        vec![
            "feed:mo0:round",
            &format!("feed:mo0:tx:{}", latest - 21),
            &format!("feed:mo0:tx:{latest}")
        ]
    );
}

/// The creation snapshot's attribute names beyond the schema snapshot's, for the record.
#[test]
fn verify_creation_snapshot_extras_versus_schema() {
    let creation = fixture("creation");
    let cfg = config();
    let deployments: HashMap<Address, (crate::flamm::Role, Word)> = creation["codehashes"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(a, h)| {
            let codehash = keys::parse_word(h.as_str().unwrap()).unwrap();
            let role = *cfg.deployments.get(&codehash)?;
            Some((parse_address(a).unwrap(), (role, codehash)))
        })
        .collect();
    let deployment = |a: &Address| deployments.get(a).copied();
    let before = words_map(&creation["store_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    let block = testdata::fixture_block(&creation, vec![]);
    let components = components_in_block(&block, &cfg, &deployment, first_word);
    let changes = protocol_changes(&block, &cfg, vec![], &components, first_word);
    let id = components.tx_components[0].components[0]
        .id
        .clone();
    let got = attrs_of(&changes.changes[0], &id);
    let snap = fixture("snapshot");
    let schema: std::collections::BTreeSet<String> = snap["attributes"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    // Beyond the schema snapshot's names: the `kind` of each feed (an addition the README names),
    // the seeded ring of the DualAggregator at creation (the snapshot's ring is the one at
    // 51302915), and FLAMM-owned words the pool's own creation wrote and later cleared or that the
    // snapshot tool did not enumerate. None of them is a `feed:mo0:answer` / `started_at` /
    // `updated_at`.
    let extras: Vec<&String> = got
        .keys()
        .filter(|k| !schema.contains(*k))
        .collect();
    for e in &extras {
        assert!(
            e.ends_with(":kind") ||
                e.starts_with("feed:mo0:tx:") ||
                e.starts_with("pool:") ||
                e.starts_with("router:") ||
                e.starts_with("hook:") ||
                e.starts_with("account:"),
            "unexpected creation attribute {e}"
        );
    }
    assert_eq!(
        extras
            .iter()
            .filter(|e| e.ends_with(":kind"))
            .count(),
        4
    );
    // Absent at creation: the ring of the snapshot's block, and the FLAMM-owned words first
    // written after creation (absent == zero for a contract tracked from its creation).
    for m in schema
        .iter()
        .filter(|k| !got.contains_key(*k))
    {
        assert!(
            m.starts_with("feed:mo0:tx:") ||
                m.starts_with("pool:") ||
                m.starts_with("router:") ||
                m.starts_with("hook:") ||
                m.starts_with("spread:") ||
                m.starts_with("account:") ||
                m.starts_with("pricefeed:") ||
                m.starts_with("factory:"),
            "schema attribute {m} absent at creation"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Rotation deletions against the indexer's storage semantics.
// ---------------------------------------------------------------------------------------------

/// The attribute names the package emits for the live pool at creation, from the creation fixture
/// replay (the rows that exist in the indexer's `protocol_state` table after the creation block).
fn creation_attribute_names() -> std::collections::BTreeSet<String> {
    let creation = fixture("creation");
    let cfg = config();
    let deployments: HashMap<Address, (crate::flamm::Role, Word)> = creation["codehashes"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(a, h)| {
            let codehash = keys::parse_word(h.as_str().unwrap()).unwrap();
            let role = *cfg.deployments.get(&codehash)?;
            Some((parse_address(a).unwrap(), (role, codehash)))
        })
        .collect();
    let deployment = |a: &Address| deployments.get(a).copied();
    let before = words_map(&creation["store_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    let block = testdata::fixture_block(&creation, vec![]);
    let components = components_in_block(&block, &cfg, &deployment, first_word);
    let changes = protocol_changes(&block, &cfg, vec![], &components, first_word);
    let id = components.tx_components[0].components[0]
        .id
        .clone();
    attrs_of(&changes.changes[0], &id)
        .into_keys()
        .collect()
}

/// A proxy rotation (slot 2 write) of feed `role` to `new_agg`, replayed on the live pool with
/// the seeds as the store: the attribute changes it emits on the swap component.
fn rotation_changes(role: &str, new_agg: Address) -> BTreeMap<String, (Vec<u8>, i32)> {
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let proxy = pool.feed(role).unwrap().proxy;
    let mut phase = [0u8; 32];
    phase[30..].copy_from_slice(&9u16.to_be_bytes());
    phase[10..30].copy_from_slice(&new_agg);
    let tx = tx_with(2, &proxy, vec![], vec![storage_change(&proxy, &keys::slot(2), &phase, 1)]);
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    attrs_of(&changes.changes[0], &pool.component_ids()[0])
}

/// Every attribute a rotation deletes must exist in the indexer's state, i.e. be one the package
/// emitted before: `tycho-storage` versions a `Deletion` by removing the row from the latest set
/// and returns `StorageError::Unexpected("Missing deleted row …")` when there is none
/// (`crates/tycho-storage/src/postgres/versioning.rs`, `set_partitioned_versioning_attributes`,
/// `VersioningEntry::Deletion` arm), which fails the whole block's DB write and halts the
/// extractor at that block. The rows that exist for the live pool right after its creation are
/// the creation snapshot's names, and after the swap block the schema snapshot's set. Rotations
/// to an unlisted aggregator delete the most: the new aggregator contributes nothing beyond
/// `aggregator` / `phase`.
#[test]
fn verify_rotation_deletes_only_attributes_that_exist() {
    let mut existing = creation_attribute_names();
    let snap = fixture("snapshot");
    existing.extend(
        snap["attributes"]
            .as_object()
            .unwrap()
            .keys()
            .cloned(),
    );
    let mut offending: Vec<String> = Vec::new();
    for role in ["asset", "loan0", "seq", "mo0"] {
        let got = rotation_changes(role, [0x99u8; 20]);
        for (name, (_, change)) in &got {
            if *change == i32::from(ChangeType::Deletion) && !existing.contains(name) {
                offending.push(name.clone());
            }
        }
    }
    assert!(
        offending.is_empty(),
        "deletions of attributes that never existed (each fails the block's DB write): {offending:?}"
    );
}

/// The same rule on a rotation to a listed aggregator: the new OCR2 aggregator's rounds replace
/// the old one's, and only the access-list word the new aggregator lacks is deleted (an OCR2
/// feed never had a `secondary_round` or a `cutoff`, so neither is named).
#[test]
fn verify_rotation_to_a_listed_ocr2_aggregator_deletes_only_existing_names() {
    let existing = creation_attribute_names();
    let new_agg = parse_address("0x68be4c50235205ede361ac8244b1ee221cdda5e2").unwrap();
    let got = rotation_changes("asset", new_agg);
    let offending: Vec<&String> = got
        .iter()
        .filter(|(name, (_, change))| {
            *change == i32::from(ChangeType::Deletion) && !existing.contains(*name)
        })
        .map(|(name, _)| name)
        .collect();
    assert!(offending.is_empty(), "deletions of attributes that never existed: {offending:?}");
    let deleted: Vec<&String> = got
        .iter()
        .filter(|(_, (_, change))| *change == i32::from(ChangeType::Deletion))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(deleted, vec!["feed:asset:access_list"]);
}

// ---------------------------------------------------------------------------------------------
// Multi-block histories against the indexer's storage rule.
// ---------------------------------------------------------------------------------------------

/// The rows the indexer holds for one component, replayed from the package's changes with
/// `tycho-storage`'s rule: a `Creation` or `Update` inserts or replaces the row, a `Deletion`
/// must find one (`crates/tycho-storage/src/postgres/versioning.rs`,
/// `set_partitioned_versioning_attributes`, `VersioningEntry::Deletion`: a missing row is
/// `StorageError::Unexpected("Missing deleted row …")`, which fails the block's write). The
/// replay is stricter than the storage, since the package knows the before-state of every
/// attribute: a `Creation` must be new and an `Update` must replace a row. `tycho-indexer`'s
/// revert path depends on it (`extractor/protocol_extractor.rs`, `AttrRevert`): a reverted
/// `Update` is restored from the row's prior value, which an `Update` of a never-held row does
/// not have (an attribute miss, reverted as a deletion), and a reverted `Creation` deletes the
/// row, which loses the prior value of a held one.
#[derive(Default, Clone)]
struct Rows(BTreeMap<String, Vec<u8>>);

impl Rows {
    fn apply(&mut self, changes: &BlockChanges, component_id: &str) {
        for tx in &changes.changes {
            let index = tx.tx.as_ref().unwrap().index;
            for e in tx
                .entity_changes
                .iter()
                .filter(|e| e.component_id == component_id)
            {
                for a in &e.attributes {
                    if a.change == i32::from(ChangeType::Deletion) {
                        assert!(
                            self.0.remove(&a.name).is_some(),
                            "tx {index}: Deletion of `{}`, a row the indexer does not hold",
                            a.name
                        );
                        continue;
                    }
                    if a.change == i32::from(ChangeType::Creation) {
                        assert!(
                            !self.0.contains_key(&a.name),
                            "tx {index}: Creation of `{}`, a row the indexer already holds",
                            a.name
                        );
                    } else {
                        assert!(
                            self.0.contains_key(&a.name),
                            "tx {index}: Update of `{}`, a row the indexer does not hold",
                            a.name
                        );
                    }
                    self.0
                        .insert(a.name.clone(), a.value.clone());
                }
            }
        }
    }

    /// The feed's rows, minus the tracked proxy word (`access_controller`) that is not part of
    /// `feed_state`.
    fn feed(&self, role: &str) -> BTreeMap<String, Vec<u8>> {
        let prefix = format!("feed:{role}:");
        self.0
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix) && !k.ends_with(":access_controller"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Synthetic blocks over the live pool, from its creation: the words store starts as `store_words`
/// leaves it after the creation block (the seeds, the words the creation block wrote and those
/// its contracts' deployments wrote before) and takes every tracked write of each block; the rows
/// start as the creation snapshot. After every block the feed rows must equal `feed_state` of the
/// store, the invariant that makes every `Deletion` name an existing row.
struct Chain {
    cfg: Config,
    pool: PoolConfig,
    roles: HashMap<Address, Role>,
    store: HashMap<(Address, Word), Word>,
    rows: Rows,
    number: u64,
    ts: u64,
}

impl Chain {
    fn new(cfg: Config) -> Self {
        let creation = fixture("creation");
        let deployments: HashMap<Address, (Role, Word)> = creation["codehashes"]
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(a, h)| {
                let codehash = keys::parse_word(h.as_str().unwrap()).unwrap();
                let role = *cfg.deployments.get(&codehash)?;
                Some((parse_address(a).unwrap(), (role, codehash)))
            })
            .collect();
        let deployment = |a: &Address| deployments.get(a).copied();
        let mut store = words_map(&fixture("seeds")["words"]);
        store.extend(words_map(&creation["store_before"]));
        let first_word = |a: &Address, k: &Word| store.get(&(*a, *k)).copied();
        let block = testdata::fixture_block(&creation, vec![]);
        let components = components_in_block(&block, &cfg, &deployment, first_word);
        let changes = protocol_changes(&block, &cfg, vec![], &components, first_word);
        let pool = live_pool();
        let mut rows = Rows::default();
        rows.apply(&changes, &pool.component_ids()[0]);
        let roles: HashMap<Address, Role> = deployments
            .iter()
            .map(|(a, (r, _))| (*a, *r))
            .collect();
        let mut chain = Chain {
            cfg,
            pool,
            roles,
            store,
            rows,
            number: creation["block"].as_u64().unwrap(),
            ts: hex_u64(&creation["header"]["timestamp"]),
        };
        chain.absorb(&block);
        chain.check();
        chain
    }

    /// What `store_words` keeps of a block.
    fn absorb(&mut self, block: &substreams_ethereum::pb::eth::v2::Block) {
        let roles = self.roles.clone();
        for w in tracked_writes(block, &self.cfg, |a| roles.get(a).copied()) {
            self.store
                .insert((w.address, w.key), w.value);
        }
    }

    fn check(&self) {
        for feed in &self.pool.feeds {
            let derived =
                feeds::feed_state(&feed.role, &feed.proxy, &self.cfg.aggregators, |a, k| {
                    self.store.get(&(*a, *k)).copied()
                });
            assert_eq!(self.rows.feed(&feed.role), derived, "feed {} rows", feed.role);
        }
    }

    /// One block of transactions, each `(to, writes)`, replayed and absorbed; the rows are
    /// checked against the store afterwards.
    fn block(&mut self, txs: Vec<Tx>) -> BlockChanges {
        self.number += 1;
        self.ts += 2;
        let txs: Vec<_> = txs
            .into_iter()
            .enumerate()
            .map(|(i, (to, writes))| {
                let writes = writes
                    .iter()
                    .enumerate()
                    .map(|(j, (a, k, v))| storage_change(a, k, v, 10 * i as u64 + j as u64 + 1))
                    .collect();
                tx_with(i as u32 + 1, &to, vec![], writes)
            })
            .collect();
        let block = testdata::block(self.number, self.ts, vec![4; 32], vec![5; 32], txs);
        let empty = BlockTransactionProtocolComponents::default();
        let changes =
            protocol_changes(&block, &self.cfg, vec![self.pool.clone()], &empty, |a, k| {
                self.store.get(&(*a, *k)).copied()
            });
        let [swap, lever] = self.pool.component_ids();
        let mut lever_rows = self.rows.clone();
        self.rows.apply(&changes, &swap);
        lever_rows.apply(&changes, &lever);
        assert_eq!(self.rows.0, lever_rows.0, "both components carry the same attributes");
        self.absorb(&block);
        self.check();
        changes
    }

    fn feed(&self, role: &str) -> (Address, Address) {
        let proxy = self.pool.feed(role).unwrap().proxy;
        let phase = self.store[&(proxy, keys::slot(2))];
        (proxy, feeds::phase_and_aggregator(&phase).1)
    }

    fn rotation(&self, role: &str, to: &Address, phase: u16) -> Tx {
        let (proxy, _) = self.feed(role);
        let mut word = [0u8; 32];
        word[30..].copy_from_slice(&phase.to_be_bytes());
        word[10..30].copy_from_slice(to);
        (proxy, vec![(proxy, keys::slot(2), word)])
    }

    fn names(&self, role: &str) -> BTreeSet<String> {
        self.rows
            .feed(role)
            .into_keys()
            .collect()
    }
}

/// One transaction of a synthetic block: `(to, writes)`.
type Tx = (Address, Vec<(Address, Word, Word)>);

fn set_field(w: &mut Word, byte_offset: usize, width: usize, value: u64) {
    let end = 32 - byte_offset;
    let bytes = value.to_be_bytes();
    w[end - width..end].copy_from_slice(&bytes[8 - width..]);
}

fn word_of(chain: &Chain, address: &Address, key: &Word) -> Word {
    chain.store[&(*address, *key)]
}

/// An OCR2 transmit (`OCR2Aggregator._report` then `transmit`: `s_transmissions[round]`, then
/// `s_hotVars`).
fn ocr2_transmit(chain: &Chain, agg: &Address, round: u32, ts: u32) -> Tx {
    let mut hotvars = word_of(chain, agg, &keys::slot(11));
    set_field(&mut hotvars, 6, 4, round as u64);
    let packed =
        feeds::pack_transmission(&keys::word_from_u64(0x700000000000 + round as u64), ts - 5, ts);
    (
        *agg,
        vec![
            (
                *agg,
                FeedKind::Ocr2
                    .transmission(round)
                    .unwrap(),
                packed,
            ),
            (*agg, keys::slot(11), hotvars),
        ],
    )
}

/// A `DualAggregator` transmit: primary, or secondary-path (`isSecondary`, both round ids move).
fn dual_transmit(chain: &Chain, agg: &Address, round: u32, ts: u32, secondary: bool) -> Tx {
    let mut hotvars = chain
        .store
        .get(&(*agg, keys::slot(13)))
        .copied()
        .unwrap_or([0u8; 32]);
    set_field(&mut hotvars, 6, 4, round as u64);
    if secondary {
        set_field(&mut hotvars, 10, 4, round as u64);
    }
    let packed =
        feeds::pack_transmission(&keys::word_from_u64(0x700000000000 + round as u64), ts - 5, ts);
    (
        *agg,
        vec![
            (
                *agg,
                FeedKind::Dual
                    .transmission(round)
                    .unwrap(),
                packed,
            ),
            (*agg, keys::slot(13), hotvars),
        ],
    )
}

/// The cbBTC/USD and USDC/USD feeds (OCR2) and the sequencer feed through rounds, configuration
/// writes, rotations to a listed aggregator of another kind, to an unlisted one and back.
#[test]
fn verify_feed_rows_follow_the_words_through_rotations_and_rounds() {
    let mut chain = Chain::new(config());
    let (asset_proxy, asset_agg) = chain.feed("asset");
    let (_, loan_agg) = chain.feed("loan0");
    let (seq_proxy, seq_agg) = chain.feed("seq");
    let round0 = field(&word_of(&chain, &asset_agg, &keys::slot(11)), 6, 4) as u32;
    assert_eq!(round0, 0x38ad);

    // A `setConfig` rewrites `s_hotVars` (config digest, epoch) with the same round: no change.
    let mut hotvars = word_of(&chain, &asset_agg, &keys::slot(11));
    set_field(&mut hotvars, 1, 5, 0x0102030405);
    let changes = chain.block(vec![(asset_agg, vec![(asset_agg, keys::slot(11), hotvars)])]);
    assert!(changes.changes.is_empty(), "same round, nothing emitted");

    // Two transmits; the rows carry the second.
    let ts = chain.ts as u32;
    chain.block(vec![ocr2_transmit(&chain, &asset_agg, round0 + 1, ts + 2)]);
    chain.block(vec![ocr2_transmit(&chain, &asset_agg, round0 + 2, ts + 4)]);
    assert_eq!(chain.rows.0["feed:asset:round"], keys::word_from_u64(round0 as u64 + 2).to_vec());

    // Rotate the cbBTC/USD proxy to the USDC/USD aggregator: its rounds, its guard, and no access
    // list for this proxy.
    let rotation = chain.rotation("asset", &loan_agg, 3);
    let changes = chain.block(vec![rotation]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got["feed:asset:access_list"].1, i32::from(ChangeType::Deletion));
    assert!(!chain
        .names("asset")
        .contains("feed:asset:access_list"));
    // `addAccess(asset proxy)` on the new aggregator creates the access-list row.
    let key = FeedKind::Ocr2
        .access_list(&asset_proxy)
        .unwrap();
    let changes = chain.block(vec![(loan_agg, vec![(loan_agg, key, keys::word_from_u64(1))])]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(
        got["feed:asset:access_list"],
        (keys::word_from_u64(1).to_vec(), i32::from(ChangeType::Creation))
    );
    // A USDC/USD round now moves both feeds (the same aggregator sits behind both proxies).
    let ts = chain.ts as u32;
    let changes = chain.block(vec![ocr2_transmit(&chain, &loan_agg, 0x17, ts + 2)]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got.len(), 8);
    assert_eq!(got["feed:asset:round"].0, got["feed:loan0:round"].0);

    // Rotate to the sequencer feed's aggregator (a listed uptime feed): another kind, another
    // guard layout; the rounds are its `s_feedState`.
    let rotation = chain.rotation("asset", &seq_agg, 4);
    let changes = chain.block(vec![rotation]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got["feed:asset:kind"], (b"uptime".to_vec(), i32::from(ChangeType::Update)));
    assert_eq!(got["feed:asset:access_list"].1, i32::from(ChangeType::Deletion));
    assert_eq!(got["feed:asset:round"].0, chain.rows.0["feed:seq:round"]);
    // `_updateRound` moves `updatedAt` only; `_recordRound` moves all four, on both feeds.
    let mut state = word_of(&chain, &seq_agg, &keys::slot(4));
    set_field(&mut state, 19, 8, chain.ts + 2);
    let changes = chain.block(vec![(seq_agg, vec![(seq_agg, keys::slot(4), state)])]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        vec!["feed:asset:updated_at", "feed:seq:updated_at"]
    );
    set_field(&mut state, 0, 8, 0x15);
    set_field(&mut state, 10, 1, 1);
    set_field(&mut state, 11, 8, chain.ts + 1);
    set_field(&mut state, 19, 8, chain.ts + 4);
    let changes = chain.block(vec![(seq_agg, vec![(seq_agg, keys::slot(4), state)])]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got.len(), 8);
    assert_eq!(got["feed:seq:answer"].0, keys::word_from_u64(1).to_vec());

    // Rotate to an aggregator the manifest does not list: everything but aggregator / phase goes,
    // and its own writes are not tracked.
    let unlisted = [0x99u8; 20];
    let rotation = chain.rotation("asset", &unlisted, 5);
    chain.block(vec![rotation]);
    assert_eq!(
        chain.names("asset"),
        BTreeSet::from(["feed:asset:aggregator".to_string(), "feed:asset:phase".to_string()])
    );
    let mut hotvars = [0u8; 32];
    set_field(&mut hotvars, 6, 4, 9);
    let changes = chain.block(vec![(unlisted, vec![(unlisted, keys::slot(11), hotvars)])]);
    assert!(changes.changes.is_empty());
    // A second rotation away from it deletes nothing (nothing of it was ever a row) …
    let rotation = chain.rotation("asset", &asset_agg, 6);
    let changes = chain.block(vec![rotation]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert!(got
        .values()
        .all(|(_, c)| *c != i32::from(ChangeType::Deletion)));
    // … and the original aggregator comes back with the rounds it transmitted meanwhile.
    assert_eq!(chain.rows.0["feed:asset:round"], keys::word_from_u64(round0 as u64 + 2).to_vec());
    assert_eq!(chain.rows.0["feed:asset:kind"], b"ocr2".to_vec());
    assert_eq!(chain.rows.0["feed:asset:access_list"], keys::word_from_u64(1).to_vec());

    // The sequencer proxy's own rotation to an unlisted feed, then a status change nobody sees,
    // then back.
    let rotation = chain.rotation("seq", &unlisted, 2);
    chain.block(vec![rotation]);
    assert_eq!(chain.names("seq").len(), 2);
    let rotation = (seq_proxy, chain.rotation("seq", &seq_agg, 3).1);
    chain.block(vec![rotation]);
    assert_eq!(chain.rows.0["feed:seq:round"], keys::word_from_u64(0x15).to_vec());
}

/// The Morpho oracle's `DualAggregator` through primary and secondary-path rounds, a cutoff
/// change, rotations away and back (the ring is rebuilt from the store), and a listed
/// aggregator whose words are unknown until it writes them.
#[test]
fn verify_dual_feed_rows_follow_the_words_through_rotations_and_rounds() {
    let mut cfg = config();
    // A second DualAggregator the manifest lists but has no seeds for.
    let fresh = [0x77u8; 20];
    cfg.aggregators
        .insert(fresh, FeedKind::Dual);
    let mut chain = Chain::new(cfg);
    let (_, agg) = chain.feed("mo0");
    let (latest, secondary) = feeds::dual_hotvars(&word_of(&chain, &agg, &keys::slot(13)));
    assert_eq!((latest, secondary), (0xbcb, 0xbc9));
    assert_eq!(chain.names("mo0").len(), 6 + 21);

    // Twenty-two primary rounds: the seeded ring is evicted entry by entry, except the secondary
    // round 0xbc9, which stays until a reveal moves `latestSecondaryRoundId` past it.
    for i in 1..=22u32 {
        let ts = chain.ts as u32;
        let changes = chain.block(vec![dual_transmit(&chain, &agg, latest + i, ts + 2, false)]);
        let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
        let deleted: Vec<&String> = got
            .iter()
            .filter(|(_, (_, c))| *c == i32::from(ChangeType::Deletion))
            .map(|(k, _)| k)
            .collect();
        let leaving = latest + i - 21;
        let want =
            if leaving == secondary { vec![] } else { vec![format!("feed:mo0:tx:{leaving}")] };
        assert_eq!(deleted, want.iter().collect::<Vec<_>>(), "round {}", latest + i);
    }
    assert!(
        chain
            .names("mo0")
            .contains("feed:mo0:tx:3017"),
        "the secondary round stays"
    );
    assert_eq!(chain.names("mo0").len(), 6 + 22);
    // A secondary-path transmission reveals the newest round: the old secondary round leaves.
    let ts = chain.ts as u32;
    let changes = chain.block(vec![dual_transmit(&chain, &agg, latest + 23, ts + 2, true)]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got["feed:mo0:secondary_round"].0, keys::word_from_u64(latest as u64 + 23).to_vec());
    assert_eq!(got["feed:mo0:tx:3017"].1, i32::from(ChangeType::Deletion));
    assert_eq!(got[&format!("feed:mo0:tx:{}", latest + 2)].1, i32::from(ChangeType::Deletion));
    assert_eq!(chain.names("mo0").len(), 6 + 21);
    // `setCutoffTime`.
    let changes = chain.block(vec![(agg, vec![(agg, keys::slot(18), keys::word_from_u64(30))])]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got.len(), 1);
    assert_eq!(got["feed:mo0:cutoff"].0, keys::word_from_u64(30).to_vec());

    // Rotate to the fresh aggregator: only its kind is known until it writes.
    let rotation = chain.rotation("mo0", &fresh, 4);
    let changes = chain.block(vec![rotation]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(
        got.values()
            .filter(|(_, c)| *c == i32::from(ChangeType::Deletion))
            .count(),
        3 + 21
    );
    assert_eq!(
        chain.names("mo0"),
        BTreeSet::from([
            "feed:mo0:aggregator".to_string(),
            "feed:mo0:kind".to_string(),
            "feed:mo0:phase".to_string()
        ])
    );
    // Its `setConfig` writes `HotVars` with no round yet: round 0, secondary 0, empty ring.
    let mut hotvars = [0u8; 32];
    set_field(&mut hotvars, 1, 5, 0x0a0b0c0d0e);
    let changes = chain.block(vec![(fresh, vec![(fresh, keys::slot(13), hotvars)])]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    assert_eq!(got.keys().collect::<Vec<_>>(), vec!["feed:mo0:round", "feed:mo0:secondary_round"]);
    assert!(got
        .values()
        .all(|(v, c)| *v == vec![0u8; 32] && *c == i32::from(ChangeType::Creation)));
    // Its first rounds build the ring from nothing; nothing is ever evicted that was not a row.
    for r in 1..=3u32 {
        let ts = chain.ts as u32;
        let changes = chain.block(vec![dual_transmit(&chain, &fresh, r, ts + 2, r == 2)]);
        let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
        assert!(got
            .values()
            .all(|(_, c)| *c != i32::from(ChangeType::Deletion)));
        assert_eq!(got[&format!("feed:mo0:tx:{r}")].1, i32::from(ChangeType::Creation));
    }
    assert!(
        !chain
            .names("mo0")
            .contains("feed:mo0:cutoff"),
        "never written on this one"
    );
    // Rotate back: the previous aggregator's state comes from the store, including the rounds and
    // the cutoff it wrote while it was not behind the proxy; the fresh one's three rounds go.
    let ts = chain.ts as u32;
    chain.block(vec![dual_transmit(&chain, &agg, latest + 24, ts + 2, false)]);
    let rotation = chain.rotation("mo0", &agg, 5);
    let changes = chain.block(vec![rotation]);
    let got = attrs_of(&changes.changes[0], &chain.pool.component_ids()[0]);
    for r in 1..=3u32 {
        assert_eq!(got[&format!("feed:mo0:tx:{r}")].1, i32::from(ChangeType::Deletion));
    }
    assert_eq!(chain.rows.0["feed:mo0:round"], keys::word_from_u64(latest as u64 + 24).to_vec());
    assert_eq!(chain.rows.0["feed:mo0:cutoff"], keys::word_from_u64(30).to_vec());
    assert_eq!(chain.names("mo0").len(), 6 + 21);
    // And to an unlisted aggregator, then back again.
    let rotation = chain.rotation("mo0", &[0x99u8; 20], 6);
    chain.block(vec![rotation]);
    assert_eq!(chain.names("mo0").len(), 2);
    let rotation = chain.rotation("mo0", &agg, 7);
    chain.block(vec![rotation]);
    assert_eq!(chain.names("mo0").len(), 6 + 21);
}

/// A round of the previous aggregator, the rotation and a round of the new aggregator spread over
/// the transactions of one block, in every order the chain can produce.
#[test]
fn verify_rotation_and_rounds_in_one_block_in_any_order() {
    let mut chain = Chain::new(config());
    let (_, asset_agg) = chain.feed("asset");
    let (_, loan_agg) = chain.feed("loan0");
    let ts = chain.ts as u32;
    let old_round = ocr2_transmit(&chain, &asset_agg, 0x38ae, ts + 2);
    let rotation = chain.rotation("asset", &loan_agg, 3);
    let new_round = ocr2_transmit(&chain, &loan_agg, 0x17, ts + 2);
    chain.block(vec![old_round.clone(), rotation.clone(), new_round.clone()]);
    assert_eq!(chain.rows.0["feed:asset:round"], keys::word_from_u64(0x17).to_vec());
    let back = chain.rotation("asset", &asset_agg, 4);
    let newer = ocr2_transmit(&chain, &asset_agg, 0x38af, ts + 4);
    chain.block(vec![newer.clone(), back.clone()]);
    assert_eq!(chain.rows.0["feed:asset:round"], keys::word_from_u64(0x38af).to_vec());
    // one transaction with the round's writes on both sides of the rotation
    let (_, mut writes) = ocr2_transmit(&chain, &asset_agg, 0x38b0, ts + 6);
    writes.extend(chain.rotation("asset", &loan_agg, 5).1);
    writes.extend(ocr2_transmit(&chain, &loan_agg, 0x18, ts + 6).1);
    chain.block(vec![(asset_agg, writes)]);
    assert_eq!(chain.rows.0["feed:asset:round"], keys::word_from_u64(0x18).to_vec());
    assert_eq!(chain.rows.0["feed:loan0:round"], keys::word_from_u64(0x18).to_vec());
}
