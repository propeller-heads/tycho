use std::collections::{HashMap, HashSet};

use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::{Call, StorageChange, TransactionTrace};

use super::*;

/// Base pool, factory and hook; slot values read on-chain at block 53114447.
const POOL: [u8; 20] = hex!("bb5507daae822ba89c4da61bbfdf4b49a1666bac");
const FACTORY: [u8; 20] = hex!("9CB220581b91b0C83223012FE69688b92aD07a76");
const HOOK: [u8; 20] = hex!("C0ed888AfE87ed1AcF01D9f19bB850159e3ce434");
const OTHER_POOL: [u8; 20] = [0xa2; 20];
const BLOCK: u64 = 53114439;

fn params() -> Params {
    Params { factory: FACTORY.to_vec(), hook: HOOK.to_vec() }
}

fn id(pool: &[u8]) -> String {
    pool.to_hex()
}

#[derive(Default)]
struct MockLookups {
    pools: Vec<Vec<u8>>,
    globals: HashMap<&'static str, Vec<u8>>,
}

impl MockLookups {
    fn with_pools(pools: &[&[u8]]) -> Self {
        Self {
            pools: pools
                .iter()
                .map(|pool| pool.to_vec())
                .collect(),
            ..Default::default()
        }
    }
}

impl FeeLookups for MockLookups {
    fn pool_by_config_slot(&self, slot: &[u8]) -> Option<String> {
        self.pools
            .iter()
            .find(|pool| dynamic_fee_config_slot(pool) == slot)
            .map(|pool| id(pool))
    }

    fn pool_by_swap_hook_slot(&self, slot: &[u8]) -> Option<String> {
        self.pools
            .iter()
            .find(|pool| pool_swap_hook_slot(pool) == slot)
            .map(|pool| id(pool))
    }

    fn is_pool(&self, address: &[u8]) -> bool {
        self.pools
            .iter()
            .any(|pool| pool == address)
    }

    fn pool_ids(&self) -> Vec<String> {
        self.pools
            .iter()
            .map(|pool| id(pool))
            .collect()
    }

    fn global(&self, setting: GlobalSetting) -> Option<Vec<u8>> {
        self.globals
            .get(setting.attribute())
            .cloned()
    }
}

fn write(address: &[u8], key: [u8; 32], old: &[u8], new: &[u8]) -> StorageChange {
    StorageChange {
        address: address.to_vec(),
        key: key.to_vec(),
        old_value: left_pad_word(old),
        new_value: left_pad_word(new),
        ..Default::default()
    }
}

fn left_pad_word(value: &[u8]) -> Vec<u8> {
    let mut word = vec![0u8; 32 - value.len()];
    word.extend_from_slice(value);
    word
}

fn call(caller: &[u8], address: &[u8], writes: Vec<StorageChange>) -> Call {
    Call {
        caller: caller.to_vec(),
        address: address.to_vec(),
        storage_changes: writes,
        ..Default::default()
    }
}

fn tx(index: u32, calls: Vec<Call>) -> TransactionTrace {
    TransactionTrace { hash: vec![index as u8; 32], index, status: 1, calls, ..Default::default() }
}

fn block(transactions: Vec<TransactionTrace>) -> eth::Block {
    eth::Block { number: BLOCK, transaction_traces: transactions, ..Default::default() }
}

fn attribute(changes: &[PoolAttributes], pool: &[u8], name: &str) -> Option<Vec<u8>> {
    changes
        .iter()
        .filter(|change| change.pool == id(pool))
        .flat_map(|change| change.attributes.iter())
        .filter(|attribute| attribute.name == name)
        .last()
        .map(|attribute| attribute.value.clone())
}

#[test]
fn slots_match_the_onchain_layout() {
    assert_eq!(
        dynamic_fee_config_slot(&POOL),
        hex!("0265ac9ee7ba0aa28473d3e507a5d996d328a6594e5786ec18e1ffbf917b7625")
    );
    assert_eq!(
        pool_swap_hook_slot(&POOL),
        hex!("2ee5290b84805ebe5a51e11a5265bfe8ea12da52b6b3d58688bad889251ec6ab")
    );
    assert_eq!(
        block_fee_slot(&POOL, BLOCK),
        hex!("01d05f6fe2b2634aa528283fef92fd7b5afb6b476e9e95a5378da0950bb8f56a")
    );
}

#[test]
fn decodes_the_packed_dynamic_fee_config() {
    // dynamicFeeConfig(pool) on-chain: base 250, cap 5400, scaling factor 1e8.
    let word = hex!("0000000000000000000000000000000000000000000005f5e1000015180000fa");

    let attributes: HashMap<String, Vec<u8>> =
        dynamic_fee_config_attributes(&word, ChangeType::Update)
            .into_iter()
            .map(|attribute| (attribute.name, attribute.value))
            .collect();

    assert_eq!(attributes["dfc_baseFee"], vec![0x00, 0xfa]);
    assert_eq!(attributes["dfc_feeCap"], vec![0x15, 0x18]);
    assert_eq!(attributes["dfc_scalingFactor"], vec![0x05, 0xf5, 0xe1, 0x00]);
    assert_eq!(attributes["dfc_initialFeeEnabled"], vec![0x00]);
    assert_eq!(attributes["dfc_initialFee"], vec![0x00]);
}

#[test]
fn decodes_the_shared_settings() {
    // Hook slot 2 packs secondsAgo (600) below recordCallGasLimit (250_000).
    let slot2 = hex!("0000000000000000000000000000000000000000000000000003d09000000258");
    let factory_slot9 = hex!("000000000000000000000000c0ed888afe87ed1acf01d9f19bb850159e3ce434");

    assert_eq!(GlobalSetting::SecondsAgo.decode(&slot2), vec![0x02, 0x58]);
    assert_eq!(
        GlobalSetting::DefaultFeeCap.decode(&left_pad_word(&[0xc3, 0x50])),
        vec![0x00, 0xc3, 0x50]
    );
    assert_eq!(GlobalSetting::DefaultSwapHook.decode(&factory_slot9), HOOK.to_vec());
    assert_eq!(GlobalSetting::DefaultSwapHook.zero(), vec![0u8; 20]);
    assert_eq!(
        GlobalSetting::at(&params(), &HOOK, &plain_slot(2)),
        Some(GlobalSetting::SecondsAgo)
    );
    // Slot 9 of the hook is not the factory's default hook.
    assert_eq!(GlobalSetting::at(&params(), &HOOK, &plain_slot(9)), None);
}

#[test]
fn creation_uses_current_shared_settings_and_zero_pool_settings() {
    let mut lookups = MockLookups::default();
    lookups
        .globals
        .insert("hook_seconds_ago", vec![0x02, 0x58]);

    let attributes: HashMap<String, Vec<u8>> = creation_attributes(&lookups)
        .into_iter()
        .map(|attribute| (attribute.name, attribute.value))
        .collect();

    assert_eq!(attributes["hook_seconds_ago"], vec![0x02, 0x58]);
    assert_eq!(attributes["hook_default_scaling_factor"], vec![0x00]);
    assert_eq!(attributes["default_swap_hook"], vec![0u8; 20]);
    assert_eq!(attributes["pool_swap_hook"], vec![0u8; 20]);
    assert_eq!(attributes["dfc_baseFee"], vec![0x00]);
    assert_eq!(attributes.len(), 10);
}

#[test]
fn pool_config_write_updates_that_pool() {
    let word = hex!("0000000000000000000000000000000000000000000005f5e1000015180000fa");
    let block = block(vec![tx(
        0,
        vec![call(
            &[0x99; 20],
            &HOOK,
            vec![write(&HOOK, dynamic_fee_config_slot(&POOL), &[], &word)],
        )],
    )]);

    let changes = fee_changes(
        &params(),
        &block,
        &HashMap::new(),
        &MockLookups::with_pools(&[&POOL, &OTHER_POOL]),
    );

    assert_eq!(attribute(&changes, &POOL, "dfc_baseFee"), Some(vec![0x00, 0xfa]));
    assert_eq!(attribute(&changes, &OTHER_POOL, "dfc_baseFee"), None);
}

#[test]
fn block_fee_is_recorded_for_the_calling_pool_only() {
    let fee = 450u64.to_be_bytes();
    let block = block(vec![
        tx(
            0,
            vec![call(&POOL, &HOOK, vec![write(&HOOK, block_fee_slot(&POOL, BLOCK), &[], &fee)])],
        ),
        // The same slot written in a call a pool did not make is not a swap's fee cache.
        tx(
            1,
            vec![call(
                &[0x99; 20],
                &HOOK,
                vec![write(&HOOK, block_fee_slot(&OTHER_POOL, BLOCK), &[], &fee)],
            )],
        ),
    ]);

    let changes = fee_changes(
        &params(),
        &block,
        &HashMap::new(),
        &MockLookups::with_pools(&[&POOL, &OTHER_POOL]),
    );

    assert_eq!(attribute(&changes, &POOL, "block_fee"), Some(vec![0x01, 0xc2]));
    assert_eq!(
        attribute(&changes, &POOL, "block_fee_block"),
        Some(BLOCK.to_be_bytes()[4..].to_vec())
    );
    assert_eq!(attribute(&changes, &OTHER_POOL, "block_fee"), None);
}

#[test]
fn pool_swap_hook_write_updates_that_pool() {
    let hook = [0x77; 20];
    let block = block(vec![tx(
        0,
        vec![call(
            &[0x99; 20],
            &FACTORY,
            vec![write(&FACTORY, pool_swap_hook_slot(&POOL), &[], &hook)],
        )],
    )]);

    let changes =
        fee_changes(&params(), &block, &HashMap::new(), &MockLookups::with_pools(&[&POOL]));

    assert_eq!(attribute(&changes, &POOL, "pool_swap_hook"), Some(hook.to_vec()));
}

#[test]
fn shared_setting_fans_out_to_pools_existing_by_that_transaction() {
    let new_cap = 40_000u32.to_be_bytes();
    let block = block(vec![tx(
        3,
        vec![call(&[0x99; 20], &HOOK, vec![write(&HOOK, plain_slot(1), &[0xc3, 0x50], &new_cap)])],
    )]);
    let created_in = HashMap::from([(id(&OTHER_POOL), 5u64)]);

    let changes = fee_changes(
        &params(),
        &block,
        &created_in,
        &MockLookups::with_pools(&[&POOL, &OTHER_POOL]),
    );

    assert_eq!(attribute(&changes, &POOL, "hook_default_fee_cap"), Some(vec![0x00, 0x9c, 0x40]));
    // Created later in the block: it reads the end-of-block value at creation.
    assert_eq!(attribute(&changes, &OTHER_POOL, "hook_default_fee_cap"), None);
}

#[test]
fn write_that_keeps_a_shared_setting_is_ignored() {
    // Only recordCallGasLimit changes; secondsAgo stays 600.
    let old = hex!("0000000000000000000000000000000000000000000000000003d09000000258");
    let new = hex!("0000000000000000000000000000000000000000000000000004000000000258");
    let block = block(vec![tx(
        0,
        vec![call(&[0x99; 20], &HOOK, vec![write(&HOOK, plain_slot(2), &old, &new)])],
    )]);

    let changes =
        fee_changes(&params(), &block, &HashMap::new(), &MockLookups::with_pools(&[&POOL]));

    assert!(changes.is_empty());
}

#[test]
fn reverted_calls_and_failed_transactions_are_ignored() {
    let word = hex!("00000000000000000000000000000000000000000000000000000000000000fa");
    let mut reverted =
        call(&[0x99; 20], &HOOK, vec![write(&HOOK, dynamic_fee_config_slot(&POOL), &[], &word)]);
    reverted.state_reverted = true;
    let mut failed = tx(
        1,
        vec![call(
            &[0x99; 20],
            &HOOK,
            vec![write(&HOOK, dynamic_fee_config_slot(&POOL), &[], &word)],
        )],
    );
    failed.status = 2;
    let block = block(vec![tx(0, vec![reverted]), failed]);

    let changes =
        fee_changes(&params(), &block, &HashMap::new(), &MockLookups::with_pools(&[&POOL]));

    assert!(changes.is_empty());
}

#[test]
fn changes_follow_write_order() {
    let first = hex!("00000000000000000000000000000000000000000000000000000000000000fa");
    let second = hex!("0000000000000000000000000000000000000000000000000000000000000064");
    let slot = dynamic_fee_config_slot(&POOL);
    let block = block(vec![tx(
        0,
        vec![
            call(&[0x99; 20], &HOOK, vec![write(&HOOK, slot, &[], &first)]),
            call(&[0x99; 20], &HOOK, vec![write(&HOOK, slot, &first, &second)]),
        ],
    )]);

    let changes =
        fee_changes(&params(), &block, &HashMap::new(), &MockLookups::with_pools(&[&POOL]));

    assert_eq!(attribute(&changes, &POOL, "dfc_baseFee"), Some(vec![0x64]));
    let pools: HashSet<_> = changes
        .iter()
        .map(|change| change.pool.clone())
        .collect();
    assert_eq!(pools.len(), 1);
}
