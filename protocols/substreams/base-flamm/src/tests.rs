// Copyright (c) 2026 Everlong Labs Limited
//! The modules replayed on real Base data (`testdata/`): the createPool transaction, the pool's
//! first swap, the seed words with the views that verify them, and one round of each Chainlink
//! aggregator. Store-backed handlers cannot run outside the substreams host; their pure cores are
//! exercised with in-memory lookups.
use std::collections::{BTreeMap, HashMap, HashSet};

use ethabi::ethereum_types::U256;
use serde_json::Value;
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::{ChangeType, ProtocolComponent, TransactionChanges};

use crate::{
    config::Config,
    flamm::{
        calldata::{
            self, CREATE_POOL_SELECTOR, CREATE_POOL_SIGNATURE, POOL_CREATED_SIGNATURE,
            POOL_CREATED_TOPIC,
        },
        feeds::{self, FeedKind},
        keys::{
            self, field, hex_address, hex_word, keccak256, parse_address, parse_word, Address, Word,
        },
        statics,
        words::{block_writes, deployments_in_block, WordView},
        PoolConfig, Role,
    },
    modules::{components_in_block, protocol_changes, tracked_writes},
    testdata::{self, address, fixture, hex_bytes, hex_u64, synthetic_ring, word, words_map},
};

/// The params string of `base-flamm.yaml` (the `&params` anchor).
fn manifest_params() -> String {
    let manifest = include_str!("../base-flamm.yaml");
    let start = manifest
        .find("&params \"")
        .expect("params anchor") +
        "&params \"".len();
    let end = manifest[start..]
        .find('"')
        .expect("closing quote");
    manifest[start..start + end].to_string()
}

fn config() -> Config {
    Config::parse(&manifest_params()).expect("manifest params parse")
}

fn snapshot_component() -> ProtocolComponent {
    let snap = fixture("snapshot");
    let statics: Vec<(String, Vec<u8>)> = snap["component"]["static_attributes"]
        .as_object()
        .expect("statics")
        .iter()
        .map(|(k, v)| (k.clone(), hex_bytes(v)))
        .collect();
    ProtocolComponent::new(
        snap["component"]["id"]
            .as_str()
            .unwrap(),
    )
    .with_attributes(&statics)
}

fn live_pool() -> PoolConfig {
    statics::pool_config_from_component(&snapshot_component()).expect("pool config from snapshot")
}

/// `deploy:` lookup from the creation fixture's codehashes and the manifest registry.
fn deployments() -> HashMap<Address, (Role, Word)> {
    let cfg = config();
    fixture("creation")["codehashes"]
        .as_object()
        .expect("codehashes")
        .iter()
        .filter_map(|(a, h)| {
            let codehash = parse_word(h.as_str().unwrap()).unwrap();
            let role = *cfg.deployments.get(&codehash)?;
            Some((parse_address(a).unwrap(), (role, codehash)))
        })
        .collect()
}

fn attrs_of(changes: &TransactionChanges, component_id: &str) -> BTreeMap<String, (Vec<u8>, i32)> {
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

fn canonical_type(input: &Value) -> String {
    let t = input["type"].as_str().unwrap();
    if let Some(rest) = t.strip_prefix("tuple") {
        let inner: Vec<String> = input["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(canonical_type)
            .collect();
        format!("({}){rest}", inner.join(","))
    } else {
        t.to_string()
    }
}

#[test]
fn factory_abi_matches_the_constants() {
    let abi: Value = serde_json::from_str(include_str!("../abi/FLAMMFactory.json")).unwrap();
    for entry in abi["abi"].as_array().unwrap() {
        let sig = format!(
            "{}({})",
            entry["name"].as_str().unwrap(),
            entry["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(canonical_type)
                .collect::<Vec<_>>()
                .join(",")
        );
        match entry["type"].as_str().unwrap() {
            "event" => {
                assert_eq!(sig, POOL_CREATED_SIGNATURE);
                assert_eq!(keccak256(sig.as_bytes()), POOL_CREATED_TOPIC);
            }
            "function" => {
                assert_eq!(sig, CREATE_POOL_SIGNATURE);
                assert_eq!(keccak256(sig.as_bytes())[..4], CREATE_POOL_SELECTOR);
            }
            other => panic!("unexpected abi entry {other}"),
        }
    }
    // The real creation log carries the topic.
    let creation = fixture("creation");
    let created = creation["logs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| hex_bytes(&l["topics"][0]) == POOL_CREATED_TOPIC)
        .count();
    assert_eq!(created, 1);
}

#[test]
fn manifest_params_are_the_fixture_values() {
    let cfg = config();
    let seeds = fixture("seeds");
    assert_eq!(
        cfg.words,
        words_map(&seeds["words"]),
        "seed words differ from testdata/seeds_51154965.json"
    );
    let codehashes = fixture("creation")["codehashes"].clone();
    let expect = |role: Role, address: &str| {
        let h = parse_word(codehashes[address].as_str().unwrap()).unwrap();
        assert_eq!(cfg.deployments.get(&h), Some(&role), "{role} codehash");
    };
    expect(Role::Implementation, "0xaad580beaa2cbd8ab5f3956a5c56eda1d5ee7184");
    expect(Role::Pool, "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572");
    expect(Role::Hook, "0x65cbd227cbc61248ae77a5fc813a29c54c092134");
    expect(Role::LeverageHook, "0xe0a98d8e60035832b8bad7f7af7b9b0b3a7308f3");
    expect(Role::SpreadHook, "0x04988af54ec88d2de77b191025eaef2fe488f93b");
    expect(Role::Router, "0x19a9b39e6710aad109c829294b0841f0851c6bb4");
    expect(Role::PriceFeed, "0xbed275459578c87a63f2f50a0b077c720e838816");
    expect(Role::Factory, "0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f");
    expect(Role::Account, "0x6760e3b032ee2d670cb684d9076b8f48cb066c48");
    assert_eq!(cfg.deployments.len(), 9);
    assert!(cfg.hook_allowed(
        &parse_word(
            codehashes["0x65cbd227cbc61248ae77a5fc813a29c54c092134"]
                .as_str()
                .unwrap()
        )
        .unwrap()
    ));
    // Immutables: the getters' answers (testdata/immutables_51154990.json), addresses as 20 bytes.
    let views = fixture("immutables");
    let pool = parse_address("0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572").unwrap();
    let pinned: HashMap<String, Vec<u8>> = cfg.immutables[&pool]
        .iter()
        .cloned()
        .collect();
    for (name, value) in views["views"].as_object().unwrap() {
        let Some(pin) = pinned.get(name) else { continue };
        let raw = hex_bytes(value);
        let expected = if pin.len() == 20 { raw[12..].to_vec() } else { raw };
        assert_eq!(pin, &expected, "{name}");
    }
    assert_eq!(pinned["irm_codehash"], hex_bytes(&views["irm_codehash"]));
    assert_eq!(
        pinned["feed_mo0_secondary_proxy"],
        hex_bytes(&views["views"]["venue_0_oracle_base_feed_1"])[12..]
    );
    assert_eq!(pinned["feed_mo0_max_sync_iterations"], keys::word_from_u64(20).to_vec());
    assert_eq!(views["dual_code_has_secondary_proxy"], Value::Bool(true));
    for name in statics::REQUIRED_IMMUTABLES {
        assert!(pinned.contains_key(name), "{name}");
    }
    assert_eq!(cfg.aggregators.len(), 4);
    assert_eq!(cfg.addresses.len(), 4);
}

#[test]
fn seed_words_agree_with_the_views() {
    let seeds = fixture("seeds");
    let words = words_map(&seeds["words"]);
    let cfg = config();
    let w = |a: &str, k: &Word| words[&(parse_address(a).unwrap(), *k)];
    for (role, proxy, agg) in [
        (
            "asset",
            "0x07da0e54543a844a80abe69c8a12f22b3aa59f9d",
            "0x51ce3091cf646587e02cad83b580992f8723e718",
        ),
        (
            "loan0",
            "0x7e860098f58bbfc8648a4311b374b1d669a2bc6b",
            "0x68be4c50235205ede361ac8244b1ee221cdda5e2",
        ),
        (
            "seq",
            "0xbcf85224fc0756b9fa45aa7892530b47e10b6433",
            "0x606c6ecbd272e2174f6710b5974f23fe9899602e",
        ),
        (
            "mo0",
            "0x64c911996d3c6ac71f9b455b1e8e7266bcbd848f",
            "0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1",
        ),
    ] {
        let views = &seeds["views"][format!("feed:{role}")];
        let (phase, aggregator) = feeds::phase_and_aggregator(&w(proxy, &keys::slot(2)));
        assert_eq!(aggregator, parse_address(agg).unwrap());
        assert_eq!(hex_bytes(&views["aggregator"])[12..], aggregator);
        assert_eq!(hex_u64(&views["phaseId"]), phase as u64);
        assert_eq!(hex_bytes(&views["accessController"])[12..], w(proxy, &keys::slot(5))[12..]);
        let kind = cfg.aggregators[&aggregator];
        let latest = hex_bytes(&views["latestRoundData"]);
        let (round_id, answer, started, updated) =
            (&latest[..32], &latest[32..64], &latest[64..96], &latest[96..128]);
        match kind {
            FeedKind::Ocr2 => {
                let round = feeds::ocr2_round(&w(agg, &keys::slot(11)), |r| {
                    Some(w(agg, &kind.transmission(r).unwrap()))
                })
                .unwrap();
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(round_id)).unwrap(), 0, 8),
                    round.round as u128
                );
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(round_id)).unwrap(), 8, 2),
                    phase as u128
                );
                assert_eq!(answer, round.answer);
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(started)).unwrap(), 0, 8),
                    round.started_at as u128
                );
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(updated)).unwrap(), 0, 8),
                    round.updated_at as u128
                );
                // read access: checkEnabled word and s_accessList[proxy] against the views
                let (slot, off) = kind.check_enabled().unwrap();
                assert_eq!(field(&w(agg, &slot), off, 1) as u64, hex_u64(&views["checkEnabled"]));
                assert_eq!(
                    w(
                        agg,
                        &kind
                            .access_list(&parse_address(proxy).unwrap())
                            .unwrap()
                    )[31] as u64,
                    hex_u64(&views["hasAccess"])
                );
            }
            FeedKind::Uptime => {
                let round = feeds::uptime_round(&w(agg, &keys::slot(4))).unwrap();
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(round_id)).unwrap(), 0, 8),
                    round.round as u128
                );
                assert_eq!(answer, round.answer);
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(started)).unwrap(), 0, 8),
                    round.started_at as u128
                );
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(updated)).unwrap(), 0, 8),
                    round.updated_at as u128
                );
                let (slot, off) = kind.check_enabled().unwrap();
                assert_eq!(field(&w(agg, &slot), off, 1) as u64, hex_u64(&views["checkEnabled"]));
                assert_eq!(
                    w(
                        agg,
                        &kind
                            .access_list(&parse_address(proxy).unwrap())
                            .unwrap()
                    )[31] as u64,
                    hex_u64(&views["hasAccess"])
                );
            }
            FeedKind::Dual => {
                let (latest_round, secondary) = feeds::dual_hotvars(&w(agg, &keys::slot(13)));
                assert_eq!((latest_round, secondary), (0xbcb, 0xbc9));
                for (r, view) in
                    [(latest_round, "getRoundData_latest"), (secondary, "getRoundData_secondary")]
                {
                    let data = hex_bytes(&views[view]);
                    let (a, obs, rec) =
                        feeds::unpack_transmission(&w(agg, &kind.transmission(r).unwrap()));
                    assert_eq!(data[32..64], a);
                    assert_eq!(
                        field(&keys::parse_word(&hex::encode(&data[64..96])).unwrap(), 0, 8),
                        obs as u128
                    );
                    assert_eq!(
                        field(&keys::parse_word(&hex::encode(&data[96..128])).unwrap(), 0, 8),
                        rec as u128
                    );
                }
                assert_eq!(field(&w(agg, &keys::slot(18)), 0, 4), 10);
                // the proxy answers the latest primary round once it is older than the cutoff
                assert_eq!(
                    field(&keys::parse_word(&hex::encode(round_id)).unwrap(), 0, 8),
                    latest_round as u128
                );
            }
        }
    }
    // Morpho market and IRM words against the views.
    let morpho = "0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb";
    let market =
        parse_word("0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836").unwrap();
    let m = keys::morpho_market_keys(&market);
    let view = hex_bytes(&seeds["views"]["morpho.market"]);
    assert_eq!(
        field(&w(morpho, &m[0]), 0, 16),
        field(&keys::parse_word(&hex::encode(&view[..32])).unwrap(), 0, 16)
    );
    assert_eq!(
        field(&w(morpho, &m[0]), 16, 16),
        field(&keys::parse_word(&hex::encode(&view[32..64])).unwrap(), 0, 16)
    );
    assert_eq!(
        field(&w(morpho, &m[1]), 0, 16),
        field(&keys::parse_word(&hex::encode(&view[64..96])).unwrap(), 0, 16)
    );
    assert_eq!(
        field(&w(morpho, &m[2]), 0, 16),
        field(&keys::parse_word(&hex::encode(&view[128..160])).unwrap(), 0, 16)
    );
    assert_eq!(
        w("0x46415998764c29ab2a25cbea6254146d50d22687", &keys::irm_rate_key(&market)),
        keys::parse_word(&hex::encode(hex_bytes(&seeds["views"]["irm.rateAtTarget"]))).unwrap()
    );
}

#[test]
fn create_pool_calldata_and_log_decode() {
    let creation = fixture("creation");
    let call = calldata::decode_create_pool(&hex_bytes(&creation["tx"]["input"])).unwrap();
    assert_eq!(hex_address(&call.pool_asset), "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf");
    assert_eq!(hex_address(&call.loan_asset), "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    assert_eq!(hex_address(&call.price_feed), "0xbed275459578c87a63f2f50a0b077c720e838816");
    assert_eq!(hex_address(&call.core), "0xf39b775926b876768f215489de4f42703f153fd1");
    assert_eq!(
        hex_address(&call.hooks.invariant_hook),
        "0x65cbd227cbc61248ae77a5fc813a29c54c092134"
    );
    assert_eq!(call.hooks.fee_hook, call.hooks.invariant_hook);
    assert_eq!(call.hooks.recenter_hook, call.hooks.invariant_hook);
    assert_eq!(call.hooks.controller_hook, call.hooks.invariant_hook);
    assert_eq!(
        hex_address(&call.hooks.leverage_hook),
        "0xe0a98d8e60035832b8bad7f7af7b9b0b3a7308f3"
    );
    assert_eq!(hex_address(&call.hooks.spread_hook), "0x04988af54ec88d2de77b191025eaef2fe488f93b");
    assert_eq!(call.hooks.loan_swap_hook, [0u8; 20]);
    assert_eq!(call.venues.len(), 1);
    assert_eq!(call.venues[0].kind, 0);
    assert_eq!(
        hex_word(&call.venues[0].market_id()),
        "0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836"
    );
    let params = call.venues[0].market_params().unwrap();
    assert_eq!(hex_address(&params.oracle), "0x663becd10dae6c4a3dcd89f1d76c1174199639b9");
    assert_eq!(hex_address(&params.irm), "0x46415998764c29ab2a25cbea6254146d50d22687");
    assert_eq!(params.loan_token, call.loan_asset);
    assert_eq!(params.collateral_token, call.pool_asset);
    let log = creation["logs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| hex_bytes(&l["topics"][0]) == POOL_CREATED_TOPIC)
        .unwrap();
    let topics: Vec<Vec<u8>> = log["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(hex_bytes)
        .collect();
    let event = calldata::decode_pool_created(&topics, &hex_bytes(&log["data"])).unwrap();
    assert_eq!(hex_address(&event.pool), "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572");
    assert_eq!(event.core, call.core);
    assert_eq!(hex_address(&event.creator), "0x1ec3bb33ec8192bf8402ef4342a9ea5d2465a89d");
    assert_eq!(hex_address(&event.implementation), "0xaad580beaa2cbd8ab5f3956a5c56eda1d5ee7184");
    assert_eq!(hex_address(&event.account), "0x6760e3b032ee2d670cb684d9076b8f48cb066c48");
    assert_eq!(event.invariant_hook, call.hooks.invariant_hook);
    assert_eq!(
        hex_word(&event.invariant_codehash),
        "0x63ca81587b713df89dc9a657dae2cb5a70910cbafbe23b9ebbbdedc1bfdc3e1d"
    );
    assert_eq!(event.fee_codehash, event.invariant_codehash);
    assert_eq!(
        hex_word(&event.hook_set_hash),
        "0x6b4146f7cd2ae3767a00555254c562476c851b90babbc132dde2fdf7e0e80e0b"
    );
    assert_eq!(
        hex_word(&event.risk_hash),
        "0x010982230d4bd1462816dcbc277a406e735fa3240bef632ee2ff156c8c26af0a"
    );
}

#[test]
fn registered_code_is_recognised_at_creation() {
    let creation = fixture("creation");
    let cfg = config();
    let mut changes = Vec::new();
    for (i, (a, c)) in creation["codes"]
        .as_object()
        .unwrap()
        .iter()
        .enumerate()
    {
        changes.push(testdata::code_change(
            &parse_address(a).unwrap(),
            &hex_bytes(&c["code"]),
            i as u64 + 1,
        ));
    }
    changes.push(testdata::code_change(&[0x42u8; 20], &[0x60, 0x01, 0x60, 0x01], 99));
    let tx = testdata::transaction(testdata::TxSpec {
        index: 7,
        hash: vec![1; 32],
        from: vec![2; 20],
        to: vec![3; 20],
        input: vec![],
        logs: vec![],
        storage_changes: vec![],
        code_changes: changes,
        create: true,
    });
    let block = testdata::block(51154990, 1789099327, vec![4; 32], vec![5; 32], vec![tx]);
    let found = deployments_in_block(&block, &cfg.deployments);
    let roles: HashMap<String, Role> = found
        .iter()
        .map(|d| (hex_address(&d.address), d.role))
        .collect();
    assert_eq!(roles.len(), 3);
    assert_eq!(roles["0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572"], Role::Pool);
    assert_eq!(roles["0x6760e3b032ee2d670cb684d9076b8f48cb066c48"], Role::Account);
    assert_eq!(roles["0x04988af54ec88d2de77b191025eaef2fe488f93b"], Role::SpreadHook);
    for d in &found {
        assert_eq!(
            hex_word(&d.codehash),
            creation["codehashes"][hex_address(&d.address)]
                .as_str()
                .unwrap()
        );
    }
}

/// The creation block replayed: `map_components` then `map_protocol_changes`.
fn replay_creation() -> (Vec<ProtocolComponent>, tycho_substreams::prelude::BlockChanges) {
    let creation = fixture("creation");
    let cfg = config();
    let block = testdata::fixture_block(&creation, vec![]);
    let deployments = deployments();
    let deployment = |a: &Address| deployments.get(a).copied();
    let before = words_map(&creation["words_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    let components = components_in_block(&block, &cfg, &deployment, first_word);
    let created: Vec<ProtocolComponent> = components
        .tx_components
        .iter()
        .flat_map(|t| t.components.clone())
        .collect();
    let changes = protocol_changes(&block, &cfg, vec![], &components, first_word);
    (created, changes)
}

#[test]
fn creation_block_emits_both_components_with_the_snapshot_statics() {
    let (created, _) = replay_creation();
    assert_eq!(created.len(), 2);
    let snap = fixture("snapshot");
    let expected: BTreeMap<String, Vec<u8>> = snap["component"]["static_attributes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), hex_bytes(v)))
        .collect();
    let tokens: Vec<Vec<u8>> = snap["component"]["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(hex_bytes)
        .collect();
    // The schema snapshot lists the pool's contracts for the reader; the component carries them as
    // static attributes and lists none (the indexer resolves `contracts` against accounts the
    // stream created, and a native integration creates no accounts).
    let contracts: Vec<Vec<u8>> = snap["component"]["contract_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(hex_bytes)
        .collect();
    assert_eq!(contracts.len(), 8);
    for (component, kind) in created.iter().zip([0u64, 1u64]) {
        let got: BTreeMap<String, Vec<u8>> = component
            .static_att
            .iter()
            .map(|a| (a.name.clone(), a.value.clone()))
            .collect();
        let mut want = expected.clone();
        want.insert("component_kind".into(), keys::word_from_u64(kind).to_vec());
        assert_eq!(got, want, "static attributes of {}", component.id);
        assert_eq!(component.tokens, tokens);
        assert!(component.contracts.is_empty());
        assert!(
            component
                .id
                .starts_with(&hex_address(&contracts[0])),
            "the pool is the id"
        );
        for address in &contracts[1..] {
            assert!(
                got.values().any(|v| v == address),
                "contract {} is a static attribute",
                hex::encode(address)
            );
        }
        assert_eq!(
            component
                .protocol_type
                .as_ref()
                .unwrap()
                .name,
            "flamm_pool"
        );
        assert_eq!(component.change, i32::from(ChangeType::Creation));
    }
    assert_eq!(
        created[0].id,
        snap["component"]["id"]
            .as_str()
            .unwrap()
    );
    assert_eq!(created[1].id, snap["lever_up_id"].as_str().unwrap());
}

#[test]
fn creation_is_refused_without_allowlist_registry_or_immutables() {
    let creation = fixture("creation");
    let block = testdata::fixture_block(&creation, vec![]);
    let before = words_map(&creation["words_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    let deployments = deployments();
    let deployment = |a: &Address| deployments.get(a).copied();
    let count = |cfg: &Config, dep: &dyn Fn(&Address) -> Option<(Role, Word)>| {
        components_in_block(&block, cfg, &dep, first_word)
            .tx_components
            .len()
    };
    let mut cfg = config();
    assert_eq!(count(&cfg, &deployment), 1);
    cfg.hook_codehashes.clear();
    assert_eq!(count(&cfg, &deployment), 0, "hook not allowlisted");
    let mut cfg = config();
    cfg.immutables.clear();
    assert_eq!(count(&cfg, &deployment), 0, "no immutables entry");
    let cfg = config();
    let router = parse_address("0x19a9b39e6710aad109c829294b0841f0851c6bb4").unwrap();
    let without_router =
        |a: &Address| if *a == router { None } else { deployments.get(a).copied() };
    assert_eq!(count(&cfg, &without_router), 0, "router not created in range");
    // External words the manifest does not track: `store_words` would never keep them.
    let pool = live_pool();
    let venue = &pool.venues[0];
    let mut cfg = config();
    cfg.words
        .remove(&(venue.morpho, keys::morpho_market_keys(&venue.market_id)[0]));
    assert_eq!(count(&cfg, &deployment), 0, "Morpho market totals not seeded");
    let mut cfg = config();
    cfg.words
        .remove(&(venue.morpho, keys::morpho_position_keys(&venue.market_id, &venue.account)[1]));
    assert_eq!(count(&cfg, &deployment), 0, "Morpho position collateral not seeded");
    let mut cfg = config();
    cfg.words
        .remove(&(venue.irm, keys::irm_rate_key(&venue.market_id)));
    assert_eq!(count(&cfg, &deployment), 0, "IRM rate not seeded");
    let mut cfg = config();
    cfg.words
        .remove(&(pool.feed("seq").unwrap().proxy, keys::slot(keys::PROXY_ACCESS_CONTROLLER_SLOT)));
    assert_eq!(count(&cfg, &deployment), 0, "sequencer proxy access controller not seeded");
    let mo0_aggregator = parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap();
    let mut cfg = config();
    cfg.addresses
        .retain(|a| *a != mo0_aggregator);
    assert_eq!(count(&cfg, &deployment), 0, "DualAggregator writes not tracked");
    let mut cfg = config();
    cfg.aggregators.remove(&mo0_aggregator);
    assert_eq!(count(&cfg, &deployment), 0, "DualAggregator layout unknown");
    // and the reasons are named
    let view = crate::flamm::words::WordView::new(&[], first_word, &cfg.words);
    let untracked = statics::untracked_external_words(&pool, &cfg, &view, 0);
    assert_eq!(untracked, vec!["feed:mo0:kind (0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1)"]);
    assert!(statics::untracked_external_words(&pool, &config(), &view, 0).is_empty());
}

/// A pool created without the leverage pair (`HookSet.leverageHook == spreadHook == 0`, valid
/// per `FLAMMOpsLib._checkHookSet`, FLAMMOpsLib.sol:209) is indexed with zero hooks and zero
/// codehashes and tracks no spread hook; a pair with one zero is refused (the chain reverts
/// `HookInvalid` at `initializeHooks`, so no such `PoolCreated` exists).
#[test]
fn creation_without_the_leverage_pair_is_indexed_with_zero_hooks() {
    let creation = fixture("creation");
    let block = testdata::fixture_block(&creation, vec![]);
    let tx = &block.transaction_traces[0];
    let log = tx
        .receipt
        .as_ref()
        .unwrap()
        .logs
        .iter()
        .find(|l| l.topics.first().map(Vec::as_slice) == Some(&POOL_CREATED_TOPIC))
        .unwrap();
    let event = calldata::decode_pool_created(&log.topics, &log.data).unwrap();
    let live = calldata::decode_create_pool(&tx.input).unwrap();
    let cfg = config();
    let deployments = deployments();
    let deployment = |a: &Address| deployments.get(a).copied();
    let before = words_map(&creation["words_before"]);
    let first_word = |a: &Address, k: &Word| before.get(&(*a, *k)).copied();
    // The pool's words 24 and 25 are never written when the hooks are zero (no storage change
    // for a zero written over zero).
    let hook_words = [keys::add(&keys::FLAMM_NS, 24), keys::add(&keys::FLAMM_NS, 25)];
    let writes = block_writes(&block, |a, k| !(*a == event.pool && hook_words.contains(k)));
    let view = WordView::new(&writes, first_word, &cfg.words);
    let mut call = live.clone();
    call.hooks.leverage_hook = [0u8; 20];
    call.hooks.spread_hook = [0u8; 20];
    let created =
        statics::creation(&event, &call, &cfg, &deployment, &view, tx.index as u64).unwrap();
    assert_eq!(created.config.leverage_hook, [0u8; 20]);
    assert_eq!(created.config.spread_hook, [0u8; 20]);
    let statics: HashMap<String, Vec<u8>> = created
        .static_attributes
        .iter()
        .cloned()
        .collect();
    assert_eq!(statics["leverage_hook"], vec![0u8; 20]);
    assert_eq!(statics["spread_hook"], vec![0u8; 20]);
    assert_eq!(statics["leverage_hook_codehash"], vec![0u8; 32]);
    assert_eq!(statics["spread_hook_codehash"], vec![0u8; 32]);
    assert!(!created
        .config
        .storage_contracts()
        .iter()
        .any(|(r, _)| *r == Role::SpreadHook));
    assert!(!created
        .config
        .tracked_words()
        .iter()
        .any(|(_, _, n)| n.starts_with("spread:")));
    let [swap, lever] = statics::components(&created);
    assert_eq!(swap.static_att.len(), lever.static_att.len());
    // the live pool still indexes as before through the same view
    let with_hooks = block_writes(&block, |_, _| true);
    let view = WordView::new(&with_hooks, first_word, &cfg.words);
    assert!(statics::creation(&event, &live, &cfg, &deployment, &view, tx.index as u64).is_ok());
    // one zero, the other bound: not a valid hook set
    let mut half = live.clone();
    half.hooks.spread_hook = [0u8; 20];
    let err = statics::creation(&event, &half, &cfg, &deployment, &view, tx.index as u64)
        .unwrap_err()
        .to_string();
    assert!(err.contains("bound as a pair"), "{err}");
    // and the stored hooks still have to match the calldata
    let view = WordView::new(&writes, first_word, &cfg.words);
    let err = statics::creation(&event, &live, &cfg, &deployment, &view, tx.index as u64)
        .unwrap_err()
        .to_string();
    assert!(err.contains("differ from the createPool calldata"), "{err}");
}

#[test]
fn creation_snapshot_carries_every_tracked_word_feed_and_balance() {
    let (created, changes) = replay_creation();
    assert_eq!(changes.changes.len(), 1);
    let tx = &changes.changes[0];
    assert_eq!(tx.component_changes.len(), 2);
    let swap = attrs_of(tx, &created[0].id);
    let lever = attrs_of(tx, &created[1].id);
    assert_eq!(swap, lever, "both components carry the same attributes");
    assert!(swap
        .values()
        .all(|(_, c)| *c == i32::from(ChangeType::Creation)));

    let snap = fixture("snapshot");
    let snapshot_names: HashSet<String> = snap["attributes"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    // Every schema attribute is present, except the ring entries (rounds of a later block) and the
    // FLAMM-owned words the creation transaction never wrote (absent == zero for a contract
    // tracked since creation).
    let creation = fixture("creation");
    let written: HashSet<String> = creation["storage_diffs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| format!("{}:{}", d["address"].as_str().unwrap(), d["slot"].as_str().unwrap()))
        .collect();
    let pool = live_pool();
    let by_name: HashMap<String, (Address, Word)> = pool
        .tracked_words()
        .into_iter()
        .map(|(a, k, n)| (n, (a, k)))
        .collect();
    let before = words_map(&creation["words_before"]);
    for name in &snapshot_names {
        if name.starts_with("feed:mo0:tx:") {
            continue;
        }
        if let Some((a, k)) = by_name
            .get(name)
            .filter(|_| !name.starts_with("feed:"))
        {
            let key = format!("{}:{}", hex_address(a), hex_word(k));
            let pre = before.contains_key(&(*a, *k));
            if !written.contains(&key) && !pre && !name.starts_with("mm:0:position") {
                assert!(!swap.contains_key(name), "{name} was never written yet emitted");
                continue;
            }
        }
        assert!(swap.contains_key(name), "missing {name}");
    }
    // Values: the config words the swap left untouched equal the snapshot's.
    let snap_attrs = snap["attributes"].as_object().unwrap();
    for name in [
        "pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4500",
        "pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4507",
        "pool:0xe27b86aa3e64fe0cf7c9294fb8b6fb20a28e5f01ba99e4bca9e76b647cc44f23",
        "hook:0x0000000000000000000000000000000000000000000000000000000000000000",
        "hook:0x000000000000000000000000000000000000000000000000000000000000000a",
        "spread:0x0000000000000000000000000000000000000000000000000000000000000000",
        "router:0x627459f28fd627023883d9310c65240762faa343d3f2429d1746640d8d8a0574",
        "router:0x001189082010b9dff4cf86574fcb5fe6ac33a2e918767f233fca4e67dd4bba1d",
        "account:0x7f73fe763fd70629cadd63d534e4c70682776b4eaeffdf39178b56c0a1bffde4",
        "pricefeed:0x1df6378d90dbe801fca9d47d5375a5a229ffa4eb34516b72a9e9ff9483681050",
        "factory:0x0000000000000000000000000000000000000000000000000000000000000000",
        "factory:0xf67576777f99137ee577c518af5f53b3235ac7369b0be96b8b092f51a7007c6a",
        "mm:0:position:0",
        "feed:asset:aggregator",
        "feed:asset:phase",
        "feed:asset:access_controller",
        "feed:asset:check_enabled",
        "feed:asset:access_list",
        "feed:loan0:aggregator",
        "feed:seq:aggregator",
        "feed:seq:round",
        "feed:seq:answer",
        "feed:seq:started_at",
        "feed:mo0:aggregator",
        "feed:mo0:cutoff",
        "feed:mo0:access_controller",
    ] {
        assert_eq!(swap[name].0, hex_bytes(&snap_attrs[name]), "{name}");
    }
    // The seeded rounds (initialBlock - 1) and the ring of the DualAggregator at the seed.
    assert_eq!(swap["feed:asset:round"].0, keys::word_from_u64(0x38ad).to_vec());
    assert_eq!(swap["feed:asset:updated_at"].0, keys::word_from_u64(0x6aa37bff).to_vec());
    assert_eq!(swap["feed:loan0:round"].0, keys::word_from_u64(0x16).to_vec());
    assert_eq!(swap["feed:mo0:round"].0, keys::word_from_u64(0xbcb).to_vec());
    assert_eq!(swap["feed:mo0:secondary_round"].0, keys::word_from_u64(0xbc9).to_vec());
    assert_eq!(
        swap.keys()
            .filter(|k| k.starts_with("feed:mo0:tx:"))
            .count(),
        21
    );
    assert_eq!(swap["feed:asset:kind"].0, b"ocr2".to_vec());
    assert_eq!(swap["feed:seq:kind"].0, b"uptime".to_vec());
    assert_eq!(swap["feed:mo0:kind"].0, b"dual".to_vec());
    assert!(!swap.contains_key("feed:mo0:check_enabled"), "the DualAggregator has no read guard");
    // Balances: the seed the creator bootstrapped (physicalPoolAsset) and no loan asset yet.
    let balances: HashMap<Vec<u8>, Vec<u8>> = tx
        .balance_changes
        .iter()
        .filter(|b| b.component_id == created[0].id.as_bytes())
        .map(|b| (b.token.clone(), b.balance.clone()))
        .collect();
    assert_eq!(balances.len(), 2);
    let physical = swap["pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f450c"]
        .0
        .clone();
    assert_eq!(balances[&pool.pool_asset.to_vec()], physical);
    assert_eq!(balances[&pool.loan_assets[0].to_vec()], vec![0u8; 32]);
    assert_eq!(
        tx.balance_changes
            .iter()
            .filter(|b| b.component_id == created[1].id.as_bytes())
            .count(),
        2
    );
}

#[test]
fn swap_block_words_before_are_the_snapshot() {
    // The fixture's words at 51302915 (eth_getStorageAt) are the schema snapshot's attributes at
    // that block, keyed through this package's slot derivations.
    let swap = fixture("swap");
    let before = words_map(&swap["words_before"]);
    let snap = fixture("snapshot");
    let pool = live_pool();
    let seeds = words_map(&fixture("seeds")["words"]);
    let mut checked = 0;
    for (a, k, name) in pool.tracked_words() {
        if let Some(v) = snap["attributes"].get(&name) {
            // the proxies' access controllers are not in the swap fixture: seeded, never written
            // since
            let value = before
                .get(&(a, k))
                .or_else(|| seeds.get(&(a, k)))
                .unwrap_or_else(|| panic!("{name}"));
            let expected = hex_bytes(v);
            assert_eq!(value[32 - expected.len()..].to_vec(), expected, "{name}");
            checked += 1;
        }
    }
    assert_eq!(checked, 87 + 6 + 4, "raw words, Morpho/IRM words, proxy access controllers");
}

/// The words store as of 51302915 for the swap fixture: the fixture reads every tracked word
/// (`eth_getStorageAt`, zero included), the store records writes, so a word that reads zero was
/// never written and has no store row. (A word written back to zero would be in the store; none
/// of the pool's words was by 51302915: the e2e replay, which takes the real writes, agrees.)
fn swap_words_store() -> HashMap<(Address, Word), Word> {
    words_map(&fixture("swap")["words_before"])
        .into_iter()
        .filter(|(_, v)| !keys::is_zero(v))
        .collect()
}

#[test]
fn swap_block_updates_the_words_the_fill_moved_and_the_inventory() {
    let swap = fixture("swap");
    let cfg = config();
    let block = testdata::fixture_block(&swap, vec![]);
    let before = words_map(&swap["words_before"]);
    let store = swap_words_store();
    let first_word = |a: &Address, k: &Word| store.get(&(*a, *k)).copied();
    let pool = live_pool();
    let empty = tycho_substreams::prelude::BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, first_word);
    assert_eq!(changes.block.as_ref().unwrap().number, 51302916);
    assert_eq!(changes.block.as_ref().unwrap().ts, 1789395179);
    assert_eq!(changes.changes.len(), 1);
    let tx = &changes.changes[0];
    assert_eq!(tx.tx.as_ref().unwrap().index, 0x61);
    assert!(tx.component_changes.is_empty());
    let [swap_id, lever_id] = pool.component_ids();
    let got = attrs_of(tx, &swap_id);
    assert_eq!(got, attrs_of(tx, &lever_id));
    let by_key: HashMap<(Address, Word), String> = pool
        .tracked_words()
        .into_iter()
        .map(|(a, k, n)| ((a, k), n))
        .collect();
    // The first write of a word the indexer holds no row for is a `Creation`: the hook's word
    // 0x13 and the venue's `managedCollateral` on the Router had never been written. The
    // venue's Morpho position word is written for the first time too, but the creation
    // snapshot carried it as a zero row, so it is an `Update` like the other held rows.
    let creations = [
        "hook:0x0000000000000000000000000000000000000000000000000000000000000013",
        "router:0x001189082010b9dff4cf86574fcb5fe6ac33a2e918767f233fca4e67dd4bba20",
    ];
    let mut want = BTreeMap::new();
    for d in swap["storage_diffs"]
        .as_array()
        .unwrap()
    {
        let key = (address(&d["address"]), word(&d["slot"]));
        let name = by_key[&key].clone();
        let change = if creations.contains(&name.as_str()) {
            assert!(keys::is_zero(&word(&d["old"])) && !store.contains_key(&key), "{name}");
            ChangeType::Creation
        } else {
            assert!(store.contains_key(&key) || name == "mm:0:position:1", "{name}: a held row");
            ChangeType::Update
        };
        want.insert(name, (word(&d["new"]).to_vec(), i32::from(change)));
    }
    assert_eq!(got, want);
    assert_eq!(want.len(), 12);
    assert_eq!(want["mm:0:position:1"].1, i32::from(ChangeType::Update));
    let venue = &pool.venues[0];
    let [_, collateral_key] = keys::morpho_position_keys(&venue.market_id, &venue.account);
    assert!(keys::is_zero(&before[&(venue.morpho, collateral_key)]), "the position's first write");
    assert!(want.contains_key("irm:0:rate_at_target"));
    assert!(want
        .contains_key("pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f450c"));
    assert!(want
        .contains_key("router:0x001189082010b9dff4cf86574fcb5fe6ac33a2e918767f233fca4e67dd4bba20"));
    // Inventory after the fill: 15000 sats bought = physical after + the collateral now posted on
    // Morpho. The loan asset is the untouched liquid balance (the USDC paid out was borrowed), so
    // its balance is not re-emitted.
    let balances: HashMap<Vec<u8>, Vec<u8>> = tx
        .balance_changes
        .iter()
        .filter(|b| b.component_id == swap_id.as_bytes())
        .map(|b| (b.token.clone(), b.balance.clone()))
        .collect();
    let physical_before = field(&before[&(pool.pool, keys::add(&keys::FLAMM_NS, 12))], 0, 32);
    assert_eq!(physical_before, 0x3591f);
    assert_eq!(balances.len(), 1);
    assert_eq!(balances[&pool.pool_asset.to_vec()], keys::word_from_u64(0x32d4a + 0x666d).to_vec());
    assert_eq!(0x32d4a + 0x666d, physical_before + 15_000);
    let view = WordView::new(&[], first_word, &cfg.words);
    let liquid = before[&(pool.pool, keys::add(&keys::pool_loan(0), 5))];
    let inventory = crate::flamm::balances::balances(&pool, &view, 0).unwrap();
    assert_eq!(inventory[1], (pool.loan_assets[0], U256::from_big_endian(&liquid)));
    assert_eq!(inventory[0], (pool.pool_asset, U256::from(physical_before)));
    assert_eq!(
        tx.balance_changes
            .iter()
            .filter(|b| b.component_id == lever_id.as_bytes())
            .count(),
        1
    );
    let snap = fixture("snapshot");
    assert_eq!(hex_u64(&snap["balances"][hex_address(&pool.pool_asset)]), physical_before as u64);
}

/// The change type follows the rows the indexer holds, across the transactions of a block: the
/// first write of a word without a row is a `Creation`, its next write (a later transaction, or
/// a later block through the words store) an `Update`; two writes in one transaction resolve to
/// one attribute with the last value. `tycho-indexer` restores a reverted `Update` from the
/// row's prior value and reverts a `Creation` by deleting the row (`protocol_extractor.rs`,
/// `AttrRevert`), so an `Update` of a row it never held is an attribute miss.
#[test]
fn first_write_of_a_word_without_a_row_is_a_creation_and_the_next_an_update() {
    let swap = fixture("swap");
    let cfg = config();
    let pool = live_pool();
    let store = swap_words_store();
    let hook_word = keys::slot(0x13);
    let hook_name = "hook:0x0000000000000000000000000000000000000000000000000000000000000013";
    assert!(!store.contains_key(&(pool.hook, hook_word)));
    let write = |ordinal: u64, value: u64| StorageChange {
        address: pool.hook.to_vec(),
        key: hook_word.to_vec(),
        old_value: Vec::new(),
        new_value: keys::word_from_u64(value).to_vec(),
        ordinal,
    };
    let tx = |index: u32, writes: Vec<StorageChange>| {
        testdata::transaction(testdata::TxSpec {
            index,
            hash: vec![index as u8; 32],
            from: vec![1u8; 20],
            to: pool.pool.to_vec(),
            input: Vec::new(),
            logs: Vec::new(),
            storage_changes: writes,
            code_changes: Vec::new(),
            create: false,
        })
    };
    let header = &swap["header"];
    let block = testdata::block(
        51302916,
        hex_u64(&header["timestamp"]),
        hex_bytes(&header["hash"]),
        hex_bytes(&header["parentHash"]),
        vec![tx(3, vec![write(1, 7), write(2, 8)]), tx(5, vec![write(1, 9)])],
    );
    let empty = tycho_substreams::prelude::BlockTransactionProtocolComponents::default();
    let [swap_id, _] = pool.component_ids();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        store.get(&(*a, *k)).copied()
    });
    assert_eq!(changes.changes.len(), 2);
    let first = attrs_of(&changes.changes[0], &swap_id);
    assert_eq!(
        first[hook_name],
        (keys::word_from_u64(8).to_vec(), i32::from(ChangeType::Creation)),
        "two writes in one transaction: one Creation with the last value"
    );
    let second = attrs_of(&changes.changes[1], &swap_id);
    assert_eq!(
        second[hook_name],
        (keys::word_from_u64(9).to_vec(), i32::from(ChangeType::Update)),
        "the row exists since the earlier transaction"
    );
    // The same write in a later block: the words store holds the row.
    let block = testdata::block(
        51302917,
        hex_u64(&header["timestamp"]) + 2,
        vec![2u8; 32],
        hex_bytes(&header["hash"]),
        vec![tx(0, vec![write(1, 10)])],
    );
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        if (*a, *k) == (pool.hook, hook_word) {
            Some(keys::word_from_u64(9))
        } else {
            store.get(&(*a, *k)).copied()
        }
    });
    let later = attrs_of(&changes.changes[0], &swap_id);
    assert_eq!(later[hook_name], (keys::word_from_u64(10).to_vec(), i32::from(ChangeType::Update)));
    // A seeded word never written in the range (the IRM rate) is held from the seed.
    let venue = &pool.venues[0];
    let rate_key = keys::irm_rate_key(&venue.market_id);
    assert!(cfg
        .words
        .contains_key(&(venue.irm, rate_key)));
    let block = testdata::block(
        51302917,
        hex_u64(&header["timestamp"]) + 2,
        vec![2u8; 32],
        hex_bytes(&header["hash"]),
        vec![tx(
            0,
            vec![StorageChange {
                address: venue.irm.to_vec(),
                key: rate_key.to_vec(),
                old_value: Vec::new(),
                new_value: keys::word_from_u64(11).to_vec(),
                ordinal: 1,
            }],
        )],
    );
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |_, _| None);
    let seeded = attrs_of(&changes.changes[0], &swap_id);
    assert_eq!(
        seeded["irm:0:rate_at_target"],
        (keys::word_from_u64(11).to_vec(), i32::from(ChangeType::Update))
    );
}

#[test]
fn words_store_keeps_tracked_writes_only() {
    let swap = fixture("swap");
    let cfg = config();
    let mut extra = testdata::storage_changes(&swap["storage_diffs"], 1);
    let pool = live_pool();
    let n = extra.len() as u64;
    // an LP's share balance (ERC-20 `_balances[holder]`): a pool word outside the namespace ranges
    let balances_slot = keys::map_slot(&keys::word_from_address(&[0x77u8; 20]), &keys::ERC20_NS);
    extra.push(StorageChange {
        address: pool.pool.to_vec(),
        key: balances_slot.to_vec(),
        old_value: vec![],
        new_value: vec![1],
        ordinal: n + 1,
    });
    // another Morpho market: not seeded
    extra.push(StorageChange {
        address: pool.venues[0].morpho.to_vec(),
        key: [0x55u8; 32].to_vec(),
        old_value: vec![],
        new_value: vec![1],
        ordinal: n + 2,
    });
    // the cbBTC/USD aggregator: every write tracked (`addresses`)
    let asset_agg = parse_address("0x51ce3091cf646587e02cad83b580992f8723e718").unwrap();
    extra.push(StorageChange {
        address: asset_agg.to_vec(),
        key: keys::transmission_key(99_999, 12).to_vec(),
        old_value: vec![],
        new_value: vec![1],
        ordinal: n + 3,
    });
    // the hook's slot 7: a registered contract, every slot tracked
    extra.push(StorageChange {
        address: pool.hook.to_vec(),
        key: keys::slot(7).to_vec(),
        old_value: vec![],
        new_value: vec![1],
        ordinal: n + 4,
    });
    let tx = testdata::transaction(testdata::TxSpec {
        index: 1,
        hash: vec![1; 32],
        from: vec![2; 20],
        to: pool.pool.to_vec(),
        input: vec![],
        logs: vec![],
        storage_changes: extra,
        code_changes: vec![],
        create: false,
    });
    let block = testdata::block(51302916, 1789395179, vec![4; 32], vec![5; 32], vec![tx]);
    let deployments = deployments();
    let kept = tracked_writes(&block, &cfg, |a| deployments.get(a).map(|(r, _)| *r));
    assert_eq!(kept.len(), 12 + 2);
    assert!(kept
        .iter()
        .any(|w| w.address == asset_agg));
    assert!(kept
        .iter()
        .any(|w| w.address == pool.hook && w.key == keys::slot(7)));
    assert!(!kept
        .iter()
        .any(|w| w.key == balances_slot));
    assert!(!kept
        .iter()
        .any(|w| w.key == [0x55u8; 32]));
    assert!(kept.iter().all(|w| w.value.len() == 32));
}

/// One aggregator round from the fixture replayed as a block against the live pool: the
/// transaction carries the round's logs and the writes of the aggregator's words after it (the
/// aggregators write their words before emitting: `OCR2Aggregator._report`,
/// `DualAggregator.sol:931-964`, `OptimismSequencerUptimeFeed._recordRound`). The package reads
/// the writes; the logs are what the tests decode to check the layouts.
fn replay_feed_round(
    set: &str,
    first_word: &HashMap<(Address, Word), Word>,
    writes: Vec<(Address, Word, Word)>,
) -> BTreeMap<String, (Vec<u8>, i32)> {
    let logs = fixture("feed_logs");
    let entry = &logs[set];
    let block_number = entry["block"].as_u64().unwrap();
    let ts = hex_u64(&entry["timestamp"]);
    let logs: Vec<_> = entry["logs"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, l)| testdata::log(100 + i as u64, l))
        .collect();
    let storage_changes = writes
        .iter()
        .enumerate()
        .map(|(i, (a, k, v))| substreams_ethereum::pb::eth::v2::StorageChange {
            address: a.to_vec(),
            key: k.to_vec(),
            old_value: vec![],
            new_value: v.to_vec(),
            ordinal: 50 + i as u64,
        })
        .collect();
    let tx = testdata::transaction(testdata::TxSpec {
        index: 3,
        hash: vec![1; 32],
        from: vec![2; 20],
        to: logs[0].address.clone(),
        input: vec![],
        logs,
        storage_changes,
        code_changes: vec![],
        create: false,
    });
    let block = testdata::block(block_number, ts, vec![4; 32], vec![5; 32], vec![tx]);
    let cfg = config();
    let pool = live_pool();
    let empty = tycho_substreams::prelude::BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        first_word.get(&(*a, *k)).copied()
    });
    assert_eq!(changes.changes.len(), 1);
    let [swap_id, lever_id] = pool.component_ids();
    let got = attrs_of(&changes.changes[0], &swap_id);
    assert_eq!(got, attrs_of(&changes.changes[0], &lever_id));
    assert!(changes.changes[0]
        .balance_changes
        .is_empty());
    got
}

/// The fixture's logs of one round, decoded.
fn fixture_logs(set: &str) -> Vec<substreams_ethereum::pb::eth::v2::Log> {
    fixture("feed_logs")[set]["logs"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, l)| testdata::log(100 + i as u64, l))
        .collect()
}

#[test]
fn ocr2_rounds_follow_the_hot_words_and_agree_with_the_events() {
    let seeds = words_map(&fixture("seeds")["words"]);
    let logs = fixture("feed_logs");
    for (set, role, agg) in [
        ("asset", "asset", "0x51ce3091cf646587e02cad83b580992f8723e718"),
        ("loan0", "loan0", "0x68be4c50235205ede361ac8244b1ee221cdda5e2"),
    ] {
        let agg = parse_address(agg).unwrap();
        let entry = &logs[set];
        let hotvars = word(&entry["hotvars_after"]);
        let transmission = word(&entry["transmission_after"]);
        let round = field(&hotvars, 6, 4) as u32;
        // the transmit writes `s_transmissions[round]` and `s_hotVars`
        let got = replay_feed_round(
            set,
            &seeds,
            vec![
                (
                    agg,
                    FeedKind::Ocr2
                        .transmission(round)
                        .unwrap(),
                    transmission,
                ),
                (agg, keys::slot(11), hotvars),
            ],
        );
        let (answer, observations, recorded) = feeds::unpack_transmission(&transmission);
        assert_eq!(
            got[&format!("feed:{role}:round")].0,
            keys::word_from_u64(round as u64).to_vec()
        );
        assert_eq!(got[&format!("feed:{role}:answer")].0, answer.to_vec());
        assert_eq!(
            got[&format!("feed:{role}:started_at")].0,
            keys::word_from_u64(observations as u64).to_vec()
        );
        assert_eq!(
            got[&format!("feed:{role}:updated_at")].0,
            keys::word_from_u64(recorded as u64).to_vec()
        );
        assert_eq!(got.len(), 4, "{set}: exactly the four round attributes");
        assert!(got
            .values()
            .all(|(_, c)| *c == i32::from(ChangeType::Update)));
        // The events of the same round carry the same values (`NewTransmission.aggregatorRoundId`,
        // `.answer`, `.observationsTimestamp`; `AnswerUpdated.updatedAt` = the block's timestamp).
        let (event_round, event_answer, started_at, updated_at) =
            crate::flamm::feed_events::round_from_logs(
                &fixture_logs(set),
                hex_u64(&entry["timestamp"]),
            )
            .unwrap();
        assert_eq!((event_round, event_answer), (round as u64, answer));
        assert_eq!((started_at, updated_at), (observations as u64, recorded as u64));
        assert_eq!(recorded as u64, hex_u64(&entry["timestamp"]));
    }
}

#[test]
fn sequencer_rounds_follow_record_round_and_update_round() {
    let logs = fixture("feed_logs");
    let agg = parse_address("0x606c6ecbd272e2174f6710b5974f23fe9899602e").unwrap();
    // Status change (round 20 at 47851108): `_recordRound` rewrites `s_feedState` (round, status,
    // startedAt from the L1 message, updatedAt = block.timestamp); all four attributes move.
    let entry = &logs["seq_answer_updated"];
    let mut words = words_map(&fixture("seeds")["words"]);
    words.insert((agg, keys::slot(4)), word(&entry["feedstate_before"]));
    let after = word(&entry["feedstate_after"]);
    let got = replay_feed_round("seq_answer_updated", &words, vec![(agg, keys::slot(4), after)]);
    let state = feeds::uptime_round(&after).unwrap();
    assert_eq!(got["feed:seq:round"].0, keys::word_from_u64(state.round).to_vec());
    assert_eq!(got["feed:seq:answer"].0, state.answer.to_vec());
    assert_eq!(got["feed:seq:started_at"].0, keys::word_from_u64(state.started_at).to_vec());
    assert_eq!(got["feed:seq:updated_at"].0, keys::word_from_u64(state.updated_at).to_vec());
    assert_eq!(got.len(), 4);
    assert_eq!(state.updated_at, hex_u64(&entry["timestamp"]));
    assert_eq!(state.started_at, 1782491507);
    assert_eq!(state.updated_at, 1782491563);
    // `AnswerUpdated(status, roundId, timestamp)` of the same round: `startedAt` is the event's
    // timestamp, `updatedAt` the block's (schema 2.6.2).
    let (event_round, status, started_at, updated_at) = crate::flamm::feed_events::round_from_logs(
        &fixture_logs("seq_answer_updated"),
        hex_u64(&entry["timestamp"]),
    )
    .unwrap();
    assert_eq!((event_round, status), (state.round, state.answer));
    assert_eq!((started_at, updated_at), (state.started_at, state.updated_at));

    // Same-status refresh (47894326): `_updateRound` rewrites `updatedAt` only.
    let entry = &logs["seq_round_updated"];
    words.insert((agg, keys::slot(4)), word(&entry["feedstate_before"]));
    let after = word(&entry["feedstate_after"]);
    let got = replay_feed_round("seq_round_updated", &words, vec![(agg, keys::slot(4), after)]);
    let state = feeds::uptime_round(&after).unwrap();
    let before = feeds::uptime_round(&word(&entry["feedstate_before"])).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got["feed:seq:updated_at"].0, keys::word_from_u64(state.updated_at).to_vec());
    assert_eq!(state.updated_at, 1782577999);
    assert_eq!(before.updated_at, 1782491563);
    assert_eq!((state.round, state.started_at), (before.round, before.started_at));
    let ru = crate::flamm::feed_events::decode_round_updated(&fixture_logs("seq_round_updated")[0])
        .unwrap();
    assert_eq!((ru.status, ru.updated_at), (state.answer, state.updated_at));
}

#[test]
fn dual_aggregator_rounds_keep_the_ring_the_reveal_reads() {
    let logs = fixture("feed_logs");
    let mut words = words_map(&fixture("seeds")["words"]);
    let agg = parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap();
    // Primary round 3583: the hot words before it are those after it with the round one lower.
    let after = word(&logs["mo0_primary"]["hotvars_after"]);
    let (latest, secondary) = feeds::dual_hotvars(&after);
    assert_eq!(latest, 0xdff);
    let mut before = after;
    before[32 - 6 - 4..32 - 6].copy_from_slice(&(latest - 1).to_be_bytes());
    words.insert((agg, keys::slot(13)), before);
    synthetic_ring(&mut words, &agg, latest - 1, secondary);
    let transmission = word(&logs["mo0_primary"]["transmission_after"]);
    let got = replay_feed_round(
        "mo0_primary",
        &words,
        vec![
            (
                agg,
                FeedKind::Dual
                    .transmission(latest)
                    .unwrap(),
                transmission,
            ),
            (agg, keys::slot(13), after),
        ],
    );
    let (answer, observations, recorded) = feeds::unpack_transmission(&transmission);
    assert_eq!(got["feed:mo0:round"].0, keys::word_from_u64(latest as u64).to_vec());
    // the ring entry is the aggregator's own storage word, which carries the answer and both
    // timestamps (the schema's `mo0` set has no answer / started_at / updated_at)
    assert_eq!(got[&format!("feed:mo0:tx:{latest}")].0, transmission.to_vec());
    assert_eq!(got[&format!("feed:mo0:tx:{latest}")].1, i32::from(ChangeType::Creation));
    assert_eq!(recorded as u64, hex_u64(&logs["mo0_primary"]["timestamp"]));
    for extra in ["feed:mo0:answer", "feed:mo0:started_at", "feed:mo0:updated_at"] {
        assert!(!got.contains_key(extra), "{extra}");
    }
    // and the round that left the 21-round window is deleted (the secondary round stays)
    let evicted = format!("feed:mo0:tx:{}", latest - 21);
    assert_eq!(got[&evicted].1, i32::from(ChangeType::Deletion));
    assert!(!got.contains_key(&format!("feed:mo0:tx:{secondary}")) || secondary >= latest - 20);
    assert_eq!(got.len(), 3);
    // `NewTransmission` / `AnswerUpdated` of the round carry the word's three fields
    let (event_round, event_answer, started_at, updated_at) =
        crate::flamm::feed_events::round_from_logs(
            &fixture_logs("mo0_primary"),
            hex_u64(&logs["mo0_primary"]["timestamp"]),
        )
        .unwrap();
    assert_eq!((event_round, event_answer), (latest as u64, answer));
    assert_eq!(
        feeds::pack_transmission(&event_answer, started_at as u32, updated_at as u32),
        transmission
    );
    assert_eq!((started_at, updated_at), (observations as u64, recorded as u64));

    // Secondary reveal (`transmitSecondary` of an existing round, `DualAggregator.sol:753-758`):
    // `latestSecondaryRoundId` moves and the old secondary round leaves the window.
    let after = word(&logs["mo0_secondary"]["hotvars_after"]);
    let before = word(&logs["mo0_secondary"]["hotvars_before"]);
    let (latest_before, secondary_before) = feeds::dual_hotvars(&before);
    let (_, secondary_after) = feeds::dual_hotvars(&after);
    words.insert((agg, keys::slot(13)), before);
    synthetic_ring(&mut words, &agg, latest_before, secondary_before);
    let got = replay_feed_round("mo0_secondary", &words, vec![(agg, keys::slot(13), after)]);
    assert_eq!(
        got["feed:mo0:secondary_round"].0,
        keys::word_from_u64(secondary_after as u64).to_vec()
    );
    let stale: Vec<_> = feeds::dual_ring(latest_before, secondary_before)
        .into_iter()
        .filter(|r| !feeds::dual_ring(latest_before, secondary_after).contains(r))
        .collect();
    assert_eq!(stale, vec![secondary_before]);
    assert_eq!(got.len(), 1 + stale.len());
    for r in stale {
        assert_eq!(got[&format!("feed:mo0:tx:{r}")].1, i32::from(ChangeType::Deletion));
    }
    let event =
        crate::flamm::feed_events::decode_secondary_round(&fixture_logs("mo0_secondary")[0])
            .unwrap();
    assert_eq!(event, secondary_after);
}

#[test]
fn proxy_rotation_clears_the_old_rounds_and_reads_the_new_aggregator() {
    // The cbBTC/USD proxy rotates to the USDC/USD aggregator (a registered OCR2 whose words are
    // seeded): the state is re-derived from the new aggregator's words and diffed against the
    // old one's.
    let seeds = words_map(&fixture("seeds")["words"]);
    let cfg = config();
    let pool = live_pool();
    let proxy = pool.feed("asset").unwrap().proxy;
    let new_agg = parse_address("0x68be4c50235205ede361ac8244b1ee221cdda5e2").unwrap();
    let mut phase = [0u8; 32];
    phase[30..].copy_from_slice(&3u16.to_be_bytes());
    phase[10..30].copy_from_slice(&new_agg);
    let tx = testdata::transaction(testdata::TxSpec {
        index: 2,
        hash: vec![1; 32],
        from: vec![2; 20],
        to: proxy.to_vec(),
        input: vec![],
        logs: vec![],
        storage_changes: vec![substreams_ethereum::pb::eth::v2::StorageChange {
            address: proxy.to_vec(),
            key: keys::slot(2).to_vec(),
            old_value: vec![],
            new_value: phase.to_vec(),
            ordinal: 1,
        }],
        code_changes: vec![],
        create: false,
    });
    let block = testdata::block(51400000, 1789600000, vec![4; 32], vec![5; 32], vec![tx]);
    let empty = tycho_substreams::prelude::BlockTransactionProtocolComponents::default();
    let changes = protocol_changes(&block, &cfg, vec![pool.clone()], &empty, |a, k| {
        seeds.get(&(*a, *k)).copied()
    });
    let got = attrs_of(&changes.changes[0], &pool.component_ids()[0]);
    assert_eq!(got["feed:asset:aggregator"].0, new_agg.to_vec());
    assert_eq!(got["feed:asset:phase"].0, keys::word_from_u64(3).to_vec());
    // the new aggregator's rounds, from its seeded `HotVars` / `s_transmissions`
    assert_eq!(
        got["feed:asset:round"],
        (keys::word_from_u64(0x16).to_vec(), i32::from(ChangeType::Update))
    );
    for name in ["feed:asset:answer", "feed:asset:started_at", "feed:asset:updated_at"] {
        assert_eq!(got[name].1, i32::from(ChangeType::Update), "{name}");
    }
    // `kind` (both OCR2) and `check_enabled` (both true) are unchanged, so not re-emitted
    assert!(!got.contains_key("feed:asset:kind"));
    assert!(!got.contains_key("feed:asset:check_enabled"));
    // `s_accessList[asset proxy]` on the new aggregator is not seeded: the pair stays incomplete,
    // fail closed.
    assert_eq!(got["feed:asset:access_list"].1, i32::from(ChangeType::Deletion));
    assert!(!got.contains_key("feed:loan0:aggregator"));
    // an OCR2 feed never had a ring, a secondary round or a cutoff, so none is deleted
    assert!(!got.keys().any(|k| {
        k.starts_with("feed:asset:tx:") ||
            k == "feed:asset:secondary_round" ||
            k == "feed:asset:cutoff"
    }));
    // aggregator, phase and the four round attributes updated; access_list deleted
    assert_eq!(got.len(), 7);
    assert_eq!(
        got.values()
            .filter(|(_, c)| *c == i32::from(ChangeType::Deletion))
            .count(),
        1
    );
}

#[test]
fn pool_config_from_the_snapshot_component_matches_the_creation() {
    let (created, _) = replay_creation();
    let from_creation = statics::pool_config_from_component(&created[0]).unwrap();
    assert_eq!(from_creation, live_pool());
    assert_eq!(PoolConfig::parse(&from_creation.serialize()).unwrap(), from_creation);
    assert_eq!(from_creation.feeds.len(), 4);
    assert_eq!(
        hex_address(&from_creation.feed("mo0").unwrap().proxy),
        "0x64c911996d3c6ac71f9b455b1e8e7266bcbd848f"
    );
}

#[test]
fn word_view_reads_the_store_before_the_block() {
    let swap = fixture("swap");
    let before = words_map(&swap["words_before"]);
    let cfg = config();
    let view = WordView::new(&[], |a, k| before.get(&(*a, *k)).copied(), &cfg.words);
    let pool = live_pool();
    assert_eq!(
        field(
            &view
                .at(&pool.pool, &keys::add(&keys::FLAMM_NS, 12), 0)
                .unwrap(),
            0,
            32
        ),
        0x3591f
    );
    // a seeded word not in the store falls back to the manifest seed
    let proxy = pool.feed("asset").unwrap().proxy;
    assert!(view
        .at(&proxy, &keys::slot(2), 0)
        .is_some());
}
