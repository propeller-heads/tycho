use std::collections::HashMap;

use ethabi::{long_signature, ParamType};
use substreams_ethereum::pb::eth::v2::{Log, TransactionReceipt, TransactionTrace};

use super::*;

const VOLATILE_FACTORY: u8 = 0xf1;
const STABLE_FACTORY: u8 = 0xf2;
const VOLATILE_MODULE: u8 = 0xe1;
const STABLE_MODULE: u8 = 0xe2;
const TOKEN0: u8 = 0x10;
const TOKEN1: u8 = 0x20;

fn address(byte: u8) -> Vec<u8> {
    vec![byte; 20]
}

fn word(value: u64) -> Vec<u8> {
    let mut word = vec![0u8; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

fn address_word(address: &[u8]) -> Vec<u8> {
    let mut word = vec![0u8; 32];
    word[12..].copy_from_slice(address);
    word
}

fn pool_created_topic() -> Vec<u8> {
    let params = [ParamType::Address, ParamType::Address, ParamType::Address, ParamType::Uint(256)];
    long_signature("PoolCreated", &params)
        .as_bytes()
        .to_vec()
}

fn sync_topic() -> Vec<u8> {
    long_signature("Sync", &[ParamType::Uint(256), ParamType::Uint(256)])
        .as_bytes()
        .to_vec()
}

fn set_custom_fee_topic() -> Vec<u8> {
    long_signature("SetCustomFee", &[ParamType::Address, ParamType::Uint(256)])
        .as_bytes()
        .to_vec()
}

fn set_default_fee_topic() -> Vec<u8> {
    long_signature("SetDefaultFee", &[ParamType::Uint(256)])
        .as_bytes()
        .to_vec()
}

fn set_fee_module_topic() -> Vec<u8> {
    long_signature("SetFeeModule", &[ParamType::Address, ParamType::Uint(32)])
        .as_bytes()
        .to_vec()
}

fn set_pause_state_topic() -> Vec<u8> {
    long_signature("SetPauseState", &[ParamType::Bool])
        .as_bytes()
        .to_vec()
}

fn params() -> Params {
    Params {
        volatile_factory: address(VOLATILE_FACTORY),
        stable_factory: address(STABLE_FACTORY),
        volatile_fee_module: address(VOLATILE_MODULE),
        stable_fee_module: address(STABLE_MODULE),
    }
}

fn log(emitter: u8, topics: Vec<Vec<u8>>, data: Vec<u8>) -> Log {
    Log { address: address(emitter), topics, data, ..Default::default() }
}

fn pool_created_log(factory: u8, pool: u8) -> Log {
    log(
        factory,
        vec![pool_created_topic(), address_word(&address(TOKEN0)), address_word(&address(TOKEN1))],
        [address_word(&address(pool)), word(1)].concat(),
    )
}

fn sync_log(pool: u8, reserve0: u64, reserve1: u64) -> Log {
    log(pool, vec![sync_topic()], [word(reserve0), word(reserve1)].concat())
}

fn set_custom_fee_log(module: u8, pool: u8, fee: u64) -> Log {
    log(module, vec![set_custom_fee_topic(), address_word(&address(pool)), word(fee)], vec![])
}

fn set_default_fee_log(factory: u8, fee: u64) -> Log {
    log(factory, vec![set_default_fee_topic()], word(fee))
}

fn set_pause_state_log(factory: u8, paused: bool) -> Log {
    log(factory, vec![set_pause_state_topic(), word(u64::from(paused))], vec![])
}

fn tx(index: u32, logs: Vec<Log>) -> TransactionTrace {
    TransactionTrace {
        hash: vec![index as u8; 32],
        index,
        status: 1,
        receipt: Some(TransactionReceipt { logs, ..Default::default() }),
        ..Default::default()
    }
}

fn block(transactions: Vec<TransactionTrace>) -> eth::Block {
    eth::Block { number: 1, transaction_traces: transactions, ..Default::default() }
}

fn id(pool: u8) -> String {
    component_id(&address(pool))
}

#[derive(Default)]
struct MockLookups {
    pools: HashMap<String, ProtocolComponent>,
    kind_pools: HashMap<&'static str, Vec<String>>,
    settings: HashMap<String, Vec<u8>>,
}

impl MockLookups {
    /// Registers pools the way the stores would see them at the end of the block.
    fn with_pools(mut self, block: &eth::Block) -> Self {
        for change in pools_created(&params(), block) {
            for component in change.component_changes {
                let kind = pool_kind(&component).unwrap();
                self.kind_pools
                    .entry(kind.key())
                    .or_default()
                    .push(component.id.clone());
                self.pools
                    .insert(component.id.clone(), component);
            }
        }
        self
    }

    fn with_setting(mut self, kind: PoolKind, field: FactoryField, value: Vec<u8>) -> Self {
        self.settings
            .insert(factory_key(kind, field), value);
        self
    }

    fn with_default_settings(self) -> Self {
        self.with_setting(PoolKind::Volatile, FactoryField::DefaultFee, vec![30])
            .with_setting(PoolKind::Volatile, FactoryField::FeeModule, address(VOLATILE_MODULE))
            .with_setting(PoolKind::Stable, FactoryField::DefaultFee, vec![5])
            .with_setting(PoolKind::Stable, FactoryField::FeeModule, address(STABLE_MODULE))
    }
}

impl Lookups for MockLookups {
    fn pool(&self, id: &str) -> Option<ProtocolComponent> {
        self.pools.get(id).cloned()
    }

    fn kind_pools(&self, kind: PoolKind) -> Vec<String> {
        self.kind_pools
            .get(kind.key())
            .cloned()
            .unwrap_or_default()
    }

    fn factory_setting(&self, kind: PoolKind, field: FactoryField) -> Option<Vec<u8>> {
        self.settings
            .get(&factory_key(kind, field))
            .cloned()
    }
}

fn run(block: &eth::Block, lookups: &MockLookups) -> Vec<TransactionChanges> {
    let created = BlockChanges {
        block: None,
        changes: pools_created(&params(), block),
        storage_changes: vec![],
    };
    protocol_changes(&params(), block, created, lookups).unwrap()
}

fn attribute(
    changes: &[TransactionChanges],
    tx_index: u64,
    pool: u8,
    name: &str,
) -> Option<Vec<u8>> {
    changes
        .iter()
        .find(|change| change.tx.as_ref().unwrap().index == tx_index)?
        .entity_changes
        .iter()
        .find(|entity| entity.component_id == id(pool))?
        .attributes
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.value.clone())
}

/// Topic prefixes observed on Base (factory constructor block 52880042, CustomFeeModule logs).
#[test]
fn abi_topics_match_on_chain_events() {
    let prefix = |topic: Vec<u8>| hex::encode(&topic[..4]);
    assert_eq!(prefix(pool_created_topic()), "ebbbe9dc");
    assert_eq!(prefix(set_custom_fee_topic()), "ae468ce5");
    assert_eq!(prefix(set_default_fee_topic()), "51de95b1");
    assert_eq!(prefix(set_fee_module_topic()), "98f4b95d");
    assert_eq!(prefix(set_pause_state_topic()), "0d76538e");
    assert_eq!(prefix(sync_topic()), "cf2aa508");
}

#[test]
fn decodes_fee_module_change() {
    let log = log(
        VOLATILE_FACTORY,
        vec![set_fee_module_topic(), address_word(&address(VOLATILE_MODULE))],
        word(60_000),
    );

    assert_eq!(
        decode_factory_event(&log),
        Some((FactoryField::FeeModule, address(VOLATILE_MODULE)))
    );
}

#[test]
fn creates_pools_only_from_known_factories_with_their_kind() {
    let block = block(vec![tx(
        0,
        vec![
            pool_created_log(VOLATILE_FACTORY, 0xa1),
            pool_created_log(STABLE_FACTORY, 0xa2),
            pool_created_log(0x99, 0xa3),
        ],
    )]);

    let components: Vec<ProtocolComponent> = pools_created(&params(), &block)
        .into_iter()
        .flat_map(|change| change.component_changes)
        .collect();

    assert_eq!(components.len(), 2);
    assert_eq!(components[0].id, id(0xa1));
    assert_eq!(pool_kind(&components[0]), Some(PoolKind::Volatile));
    assert_eq!(pool_kind(&components[1]), Some(PoolKind::Stable));
    assert_eq!(components[1].tokens, vec![address(TOKEN0), address(TOKEN1)]);
    assert_eq!(
        components[1]
            .protocol_type
            .as_ref()
            .unwrap()
            .name,
        PROTOCOL_TYPE_NAME
    );
}

#[test]
fn new_pool_carries_its_factory_settings() {
    let block = block(vec![tx(3, vec![pool_created_log(STABLE_FACTORY, 0xa2)])]);
    let lookups = MockLookups::default()
        .with_pools(&block)
        .with_default_settings();

    let changes = run(&block, &lookups);

    assert_eq!(attribute(&changes, 3, 0xa2, "default_fee"), Some(vec![5]));
    assert_eq!(attribute(&changes, 3, 0xa2, "fee_module"), Some(address(STABLE_MODULE)));
    assert_eq!(attribute(&changes, 3, 0xa2, "paused"), Some(vec![0]));
    assert_eq!(attribute(&changes, 3, 0xa2, "custom_fee"), Some(vec![0]));
    assert_eq!(changes[0].component_changes.len(), 1);
}

#[test]
fn pool_creation_without_known_default_fee_fails() {
    let block = block(vec![tx(0, vec![pool_created_log(VOLATILE_FACTORY, 0xa1)])]);
    let lookups = MockLookups::default().with_pools(&block);
    let created = BlockChanges {
        block: None,
        changes: pools_created(&params(), &block),
        storage_changes: vec![],
    };

    let err = protocol_changes(&params(), &block, created, &lookups).unwrap_err();

    assert!(
        err.to_string()
            .contains("factory deployment block"),
        "{err}"
    );
}

#[test]
fn sync_updates_reserves_and_balances_of_known_pools_only() {
    let creation = block(vec![tx(0, vec![pool_created_log(VOLATILE_FACTORY, 0xa1)])]);
    let lookups = MockLookups::default()
        .with_pools(&creation)
        .with_default_settings();
    let block = block(vec![tx(1, vec![sync_log(0xa1, 100, 200), sync_log(0xb0, 1, 1)])]);

    let changes = run(&block, &lookups);

    assert_eq!(changes.len(), 1);
    assert_eq!(attribute(&changes, 1, 0xa1, "reserve0"), Some(vec![100]));
    // Signed big-endian, so 200 carries a leading zero byte.
    assert_eq!(attribute(&changes, 1, 0xa1, "reserve1"), Some(vec![0, 200]));
    let mut balances: Vec<(Vec<u8>, Vec<u8>)> = changes[0]
        .balance_changes
        .iter()
        .map(|change| (change.token.clone(), change.balance.clone()))
        .collect();
    balances.sort();
    assert_eq!(balances, vec![(address(TOKEN0), vec![100]), (address(TOKEN1), vec![0, 200])]);
}

#[test]
fn last_sync_in_a_transaction_wins() {
    let creation = block(vec![tx(0, vec![pool_created_log(VOLATILE_FACTORY, 0xa1)])]);
    let lookups = MockLookups::default()
        .with_pools(&creation)
        .with_default_settings();
    let block = block(vec![tx(1, vec![sync_log(0xa1, 1, 2), sync_log(0xa1, 7, 8)])]);

    let changes = run(&block, &lookups);

    assert_eq!(attribute(&changes, 1, 0xa1, "reserve0"), Some(vec![7]));
}

#[test]
fn custom_fee_applies_only_from_the_pools_own_fee_module() {
    let creation = block(vec![tx(
        0,
        vec![pool_created_log(VOLATILE_FACTORY, 0xa1), pool_created_log(STABLE_FACTORY, 0xa2)],
    )]);
    let lookups = MockLookups::default()
        .with_pools(&creation)
        .with_default_settings();
    let block = block(vec![tx(
        1,
        vec![
            set_custom_fee_log(VOLATILE_MODULE, 0xa1, 100),
            // The volatile module cannot set fees for a stable pool.
            set_custom_fee_log(VOLATILE_MODULE, 0xa2, 50),
            set_custom_fee_log(STABLE_MODULE, 0xb0, 1),
            set_custom_fee_log(0x99, 0xa1, 7),
        ],
    )]);

    let changes = run(&block, &lookups);

    assert_eq!(attribute(&changes, 1, 0xa1, "custom_fee"), Some(vec![100]));
    assert_eq!(attribute(&changes, 1, 0xa2, "custom_fee"), None);
    assert_eq!(changes[0].entity_changes.len(), 1);
}

#[test]
fn factory_change_fans_out_to_pools_existing_by_that_transaction() {
    let earlier = block(vec![tx(0, vec![pool_created_log(VOLATILE_FACTORY, 0xa1)])]);
    let block = block(vec![
        tx(1, vec![pool_created_log(VOLATILE_FACTORY, 0xa2)]),
        tx(
            2,
            vec![
                set_default_fee_log(VOLATILE_FACTORY, 25),
                set_pause_state_log(VOLATILE_FACTORY, true),
            ],
        ),
        tx(
            3,
            vec![pool_created_log(VOLATILE_FACTORY, 0xa3), pool_created_log(STABLE_FACTORY, 0xa4)],
        ),
    ]);
    let lookups = MockLookups::default()
        .with_pools(&earlier)
        .with_pools(&block)
        .with_default_settings();

    let changes = run(&block, &lookups);

    for pool in [0xa1, 0xa2] {
        assert_eq!(attribute(&changes, 2, pool, "default_fee"), Some(vec![25]));
        assert_eq!(attribute(&changes, 2, pool, "paused"), Some(vec![1]));
    }
    // Created after the change: it reads the end-of-block setting at creation instead.
    assert_eq!(attribute(&changes, 2, 0xa3, "default_fee"), None);
    // Other factory.
    assert_eq!(attribute(&changes, 2, 0xa4, "default_fee"), None);
}

#[test]
fn changes_are_ordered_by_transaction_index() {
    let creation = block(vec![tx(0, vec![pool_created_log(VOLATILE_FACTORY, 0xa1)])]);
    let lookups = MockLookups::default()
        .with_pools(&creation)
        .with_default_settings();
    let block = block(vec![tx(9, vec![sync_log(0xa1, 1, 1)]), tx(4, vec![sync_log(0xa1, 2, 2)])]);

    let indices: Vec<u64> = run(&block, &lookups)
        .iter()
        .map(|change| change.tx.as_ref().unwrap().index)
        .collect();

    assert_eq!(indices, vec![4, 9]);
}

#[test]
fn failed_transactions_are_ignored() {
    let mut failed = tx(0, vec![pool_created_log(VOLATILE_FACTORY, 0xa1)]);
    failed.status = 2;

    assert!(pools_created(&params(), &block(vec![failed])).is_empty());
}
