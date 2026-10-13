//! Substreams modules for Camelot V3 (Algebra V1.9) on Arbitrum One.
//!
//! Native integration: every value a swap reads is emitted as a state attribute, decoded from
//! the storage words the pool and its `DataStorageOperator` write (`globalState`, `liquidity`,
//! the `ticks` entries a `Mint` or `Burn` touched, the timepoint ring and the fee
//! configurations). Pool token balances are tracked from ERC20 `Transfer` events on both sides
//! of a transfer, so mint, burn, collect, swap, flash, community fee payments and pool-to-pool
//! transfers are all covered by the same rule.
use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, Result};
use itertools::Itertools;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{
        StoreAddBigInt, StoreGet, StoreGetProto, StoreGetRaw, StoreNew, StoreSet,
        StoreSetIfNotExists, StoreSetIfNotExistsProto, StoreSetRaw,
    },
};
use substreams_ethereum::{
    pb::eth::v2::{Block, StorageChange, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::{abi::erc20, balances::aggregate_balances_changes, prelude::*};

use crate::{
    abi::pool::events::{Burn, Mint},
    camelot::{
        attributes, contract_key, fee_config_attribute, global_state_attributes,
        initial_attributes, liquidity_attributes, operator_slot_key, pools_created, slot_number,
        slot_word, tick_attribute, tick_slot, timepoint_attribute, timepoint_header,
        timepoint_of_slot, timepoint_slots, unreachable_timepoints, word, Word,
        DATA_STORAGE_OPERATOR_ATTRIBUTE, GLOBAL_STATE_SLOT, LIQUIDITY_SLOT,
    },
    params::parse_factory,
};

/// Emits a component for every pool the factory created in the block.
#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: Block,
) -> Result<BlockTransactionProtocolComponents> {
    let factory = parse_factory(&params)?;
    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        let components = pools_created(&factory, tx)?;
        if components.is_empty() {
            continue;
        }
        tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// Stores every pool component under its pool address and under its operator address, so later
/// modules can resolve either contract to the component it belongs to.
#[substreams::handlers::store]
fn store_protocol_components(
    components: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            store.set_if_not_exists(0, contract_key(&component.id), &component);
            if let Some(operator) = component.get_attribute_value(DATA_STORAGE_OPERATOR_ATTRIBUTE) {
                store.set_if_not_exists(0, contract_key(&operator.to_hex()), &component);
            }
        }
    }
}

/// The component whose pool contract is `address`; an operator or an unknown contract yields
/// `None`.
fn pool_at(
    components: &StoreGetProto<ProtocolComponent>,
    address: &[u8],
) -> Option<ProtocolComponent> {
    let id = address.to_hex();
    components
        .get_last(contract_key(&id))
        .filter(|component| component.id == id)
}

/// Whether `address` is the `DataStorageOperator` of a tracked pool.
fn is_operator(components: &StoreGetProto<ProtocolComponent>, address: &[u8]) -> bool {
    components
        .get_last(contract_key(&address.to_hex()))
        .is_some_and(|component| {
            component.get_attribute_value(DATA_STORAGE_OPERATOR_ATTRIBUTE) == Some(address.to_vec())
        })
}

/// Emits a relative balance delta for every ERC20 transfer of a pool token into or out of a
/// tracked pool. Both sides of a transfer are handled independently, so a transfer between two
/// pools debits one and credits the other. A self-transfer changes nothing and emits nothing.
///
/// Pool events cannot replace the transfers: a swap forwards the community share of its fee to
/// the factory's vault inside the same call, and neither that amount nor the vault is part of
/// the `Swap` event, so event-derived balances would drift by the fee on every swap.
#[substreams::handlers::map]
fn map_relative_component_balances(
    block: Block,
    components: StoreGetProto<ProtocolComponent>,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    for log in block.logs() {
        let Some(transfer) = erc20::events::Transfer::match_and_decode(log.log) else {
            continue;
        };
        if transfer.from == transfer.to {
            continue;
        }
        let token = log.address();
        let movements = [
            (&transfer.from, transfer.value.clone().neg()),
            (&transfer.to, transfer.value.clone()),
        ];
        for (account, delta) in movements {
            let Some(component) = pool_at(&components, account) else {
                continue;
            };
            if !component
                .tokens
                .iter()
                .any(|t| t == token)
            {
                continue;
            }
            balance_deltas.push(BalanceDelta {
                ord: log.ordinal(),
                tx: Some(log.receipt.transaction.into()),
                token: token.to_vec(),
                delta: delta.to_signed_bytes_be(),
                component_id: component.id.into_bytes(),
            });
        }
    }
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Accumulates the relative balance deltas into absolute pool balances.
#[substreams::handlers::store]
fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

/// The storage changes of a transaction's successful calls.
fn storage_changes(tx: &TransactionTrace) -> impl Iterator<Item = &StorageChange> {
    tx.calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
}

/// Keeps the latest value of every storage word a tracked operator wrote.
///
/// `map_protocol_changes` needs it to emit a timepoint whose two words did not both change in
/// the same transaction, and to read timestamps by ring index when it prunes the ring.
#[substreams::handlers::store]
fn store_operator_slots(
    block: Block,
    components: StoreGetProto<ProtocolComponent>,
    store: StoreSetRaw,
) {
    for tx in block.transactions() {
        for change in storage_changes(tx) {
            let Some(slot) = slot_number(&change.key) else {
                continue;
            };
            if !is_operator(&components, &change.address) {
                continue;
            }
            store.set(change.ordinal, operator_slot_key(&change.address, slot), &change.new_value);
        }
    }
}

/// Storage words written by a transaction: by contract, then by key, the first old and the
/// last new value.
type WordChanges = BTreeMap<Vec<u8>, BTreeMap<Word, (Word, Word)>>;

/// The first old and the last new value of every storage word a transaction wrote, by
/// contract and key, in the order the writes happened.
fn word_changes(tx: &TransactionTrace) -> Result<WordChanges> {
    let mut changes = WordChanges::new();
    for change in storage_changes(tx).sorted_by_key(|change| change.ordinal) {
        let key = word(&change.key)?;
        let (old, new) = (word(&change.old_value)?, word(&change.new_value)?);
        changes
            .entry(change.address.clone())
            .or_default()
            .entry(key)
            .and_modify(|value| value.1 = new)
            .or_insert((old, new));
    }
    Ok(changes)
}

/// The ticks every `Mint` and `Burn` the pool emitted in the transaction touched.
///
/// Only the pool's own logs are decoded. The event signatures are Uniswap V3's, so any contract
/// can emit a log of the same shape, including one whose tick topics do not fit an `int24`.
fn ticks_touched(tx: &TransactionTrace, pool: &[u8]) -> Vec<i32> {
    let mut touched = Vec::new();
    for log in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.logs.iter())
        .filter(|log| log.address == pool)
    {
        let (bottom, top) = if let Some(mint) = Mint::match_and_decode(log) {
            (mint.bottom_tick, mint.top_tick)
        } else if let Some(burn) = Burn::match_and_decode(log) {
            (burn.bottom_tick, burn.top_tick)
        } else {
            continue;
        };
        touched.extend([bottom.to_i32(), top.to_i32()]);
    }
    touched
}

/// The attribute changes of a pool's storage writes.
fn pool_attributes(keyed: &BTreeMap<Word, (Word, Word)>, ticks: &[i32]) -> Vec<Attribute> {
    let mut out = Vec::new();
    if let Some((old, new)) = keyed.get(&slot_word(GLOBAL_STATE_SLOT)) {
        out.extend(global_state_attributes(old, new, ChangeType::Update));
    }
    if let Some((old, new)) = keyed.get(&slot_word(LIQUIDITY_SLOT)) {
        out.extend(liquidity_attributes(old, new, ChangeType::Update));
    }
    for tick in ticks.iter().unique() {
        if let Some((old, new)) = keyed.get(&tick_slot(*tick)) {
            out.extend(tick_attribute(*tick, old, new));
        }
    }
    out
}

fn stored_word(store: &StoreGetRaw, operator: &[u8], slot: u64) -> Option<Word> {
    store
        .get_last(operator_slot_key(operator, slot))
        .map(|value| word(&value).expect("operator slot values are storage words"))
}

/// The attribute changes of an operator's storage writes: fee configurations, every timepoint
/// written and the deletions of timepoints the fee can no longer reach. `stored` reads the
/// current value of one of the operator's slots: the word of a timepoint the transaction did
/// not rewrite, and timestamps by ring index for the pruning.
fn operator_attributes(
    keyed: &BTreeMap<Word, (Word, Word)>,
    stored: impl Fn(u64) -> Option<Word>,
) -> Vec<Attribute> {
    let mut out = Vec::new();
    let mut timepoints: BTreeMap<u16, [Option<(Word, Word)>; 2]> = BTreeMap::new();
    for (key, (old, new)) in keyed {
        let Some(slot) = slot_number(key) else {
            continue;
        };
        let change =
            if old.iter().all(|b| *b == 0) { ChangeType::Creation } else { ChangeType::Update };
        if let Some(attribute) = fee_config_attribute(slot, new, change) {
            out.push(attribute);
        } else if let Some((index, second)) = timepoint_of_slot(slot) {
            timepoints.entry(index).or_default()[usize::from(second)] = Some((*old, *new));
        }
    }
    for (index, words) in timepoints {
        let (first_slot, second_slot) = timepoint_slots(index);
        let first = words[0]
            .map(|(_, new)| new)
            .or_else(|| stored(first_slot))
            .unwrap_or([0u8; 32]);
        let second = words[1]
            .map(|(_, new)| new)
            .or_else(|| stored(second_slot))
            .unwrap_or([0u8; 32]);
        let change = match words[0] {
            Some((old, _)) if !timepoint_header(&old).0 => ChangeType::Creation,
            _ => ChangeType::Update,
        };
        out.push(timepoint_attribute(index, &first, &second, change));

        // A new first word is a new timepoint (the ring never rewrites the current one), so
        // the window moved and older entries may have become unreachable.
        if let Some((_, new_first)) = words[0] {
            let (initialized, timestamp) = timepoint_header(&new_first);
            if !initialized {
                continue;
            }
            let timestamp_at = |ring_index: u16| {
                stored(timepoint_slots(ring_index).0).and_then(|first| {
                    let (initialized, timestamp) = timepoint_header(&first);
                    initialized.then_some(timestamp)
                })
            };
            for unreachable in unreachable_timepoints(index, timestamp, timestamp_at) {
                out.push(Attribute {
                    name: attributes::timepoint(unreachable),
                    value: Vec::new(),
                    change: ChangeType::Deletion.into(),
                });
            }
        }
    }
    out
}

/// The attributes of a pool created in this transaction: every scalar the decoder requires,
/// starting at zero and taking the values the transaction's own storage writes gave them (the
/// constructor's `feeZto` and `feeOtz` of `BASE_FEE`, and the price, tick and first timepoint
/// when `initialize` ran in the same transaction). Everything is a creation.
fn creation_attributes(storage: Vec<Attribute>) -> Vec<Attribute> {
    let mut by_name: BTreeMap<String, Attribute> = initial_attributes()
        .into_iter()
        .map(|attribute| (attribute.name.clone(), attribute))
        .collect();
    for mut attribute in storage {
        if attribute.change != i32::from(ChangeType::Deletion) {
            attribute.change = ChangeType::Creation.into();
        }
        by_name.insert(attribute.name.clone(), attribute);
    }
    by_name.into_values().collect()
}

/// The attribute changes of one transaction by component id: the storage writes of every
/// tracked pool and operator decoded, and every pool created in the transaction completed to
/// the full set of scalars the decoder requires. `component_at` resolves a pool or operator
/// address to its component and `stored` reads an operator's storage slot.
fn transaction_attributes(
    tx: &TransactionTrace,
    created: &[ProtocolComponent],
    component_at: impl Fn(&[u8]) -> Option<ProtocolComponent>,
    stored: impl Fn(&[u8], u64) -> Option<Word>,
) -> Result<BTreeMap<String, Vec<Attribute>>> {
    let mut per_component: BTreeMap<String, Vec<Attribute>> = BTreeMap::new();
    for (address, keyed) in &word_changes(tx)? {
        let Some(component) = component_at(address) else {
            continue;
        };
        let attributes = if component.id == address.to_hex() {
            pool_attributes(keyed, &ticks_touched(tx, address))
        } else {
            operator_attributes(keyed, |slot| stored(address, slot))
        };
        if !attributes.is_empty() {
            per_component
                .entry(component.id)
                .or_default()
                .extend(attributes);
        }
    }
    for component in created {
        let storage = per_component
            .remove(&component.id)
            .unwrap_or_default();
        per_component.insert(component.id.clone(), creation_attributes(storage));
    }
    Ok(per_component)
}

/// Merges new components, absolute balances and the attribute changes decoded from pool and
/// operator storage into one `TransactionChanges` per transaction, sorted by transaction index.
#[substreams::handlers::map]
fn map_protocol_changes(
    block: Block,
    new_components: BlockTransactionProtocolComponents,
    balance_deltas: BlockBalanceDeltas,
    components: StoreGetProto<ProtocolComponent>,
    balance_store: StoreDeltas,
    operator_slots: StoreGetRaw,
) -> Result<BlockChanges> {
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();
    let mut created_by_tx: HashMap<u64, &[ProtocolComponent]> = HashMap::new();

    for tx_components in &new_components.tx_components {
        let tx = tx_components
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("component changes without a transaction"))?;
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &tx_components.components {
            builder.add_protocol_component(component);
        }
        created_by_tx.insert(tx.index, &tx_components.components);
    }

    for (_, (tx, balances)) in aggregate_balances_changes(balance_store, balance_deltas) {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for token_balances in balances.values() {
            for change in token_balances.values() {
                builder.add_balance_change(change);
            }
        }
    }

    for tx in block.transactions() {
        let created = created_by_tx
            .get(&u64::from(tx.index))
            .copied()
            .unwrap_or_default();
        let per_component = transaction_attributes(
            tx,
            created,
            |address| components.get_last(contract_key(&address.to_hex())),
            |operator, slot| stored_word(&operator_slots, operator, slot),
        )?;
        if per_component.is_empty() {
            continue;
        }
        let builder = transaction_changes
            .entry(tx.index.into())
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));
        for (component_id, attributes) in per_component {
            builder.add_entity_change(&EntityChanges { component_id, attributes });
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .into_iter()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        // Raw per-block storage changes only feed the Dynamic Contract Indexer, which a native
        // integration does not use.
        storage_changes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{Call, Log};
    use tiny_keccak::{Hasher, Keccak};

    use super::*;
    use crate::camelot::{FEE_CONFIG_OTZ_SLOT, FEE_CONFIG_ZTO_SLOT, PROTOCOL_TYPE};

    const POOL: [u8; 20] = [0xb0; 20];
    const OPERATOR: [u8; 20] = [0x0b; 20];
    const OTHER: [u8; 20] = [0x0c; 20];

    fn component() -> ProtocolComponent {
        ProtocolComponent::new(&POOL.to_hex())
            .with_tokens(&[[1u8; 20].as_slice(), [2u8; 20].as_slice()])
            .with_attributes(&[(DATA_STORAGE_OPERATOR_ATTRIBUTE, OPERATOR.as_slice())])
            .as_swap_type(PROTOCOL_TYPE, ImplementationType::Custom)
    }

    fn component_at(address: &[u8]) -> Option<ProtocolComponent> {
        (address == POOL || address == OPERATOR).then(component)
    }

    fn word_with(value: u64) -> Word {
        slot_word(value)
    }

    fn change(address: &[u8; 20], key: Word, old: Word, new: Word, ordinal: u64) -> StorageChange {
        StorageChange {
            address: address.to_vec(),
            key: key.to_vec(),
            old_value: old.to_vec(),
            new_value: new.to_vec(),
            ordinal,
        }
    }

    fn call_with(storage_changes: Vec<StorageChange>, logs: Vec<Log>) -> Call {
        Call { storage_changes, logs, ..Default::default() }
    }

    fn tx_with(calls: Vec<Call>) -> TransactionTrace {
        TransactionTrace { status: 1, calls, ..Default::default() }
    }

    fn topic(signature: &str) -> Vec<u8> {
        let mut hasher = Keccak::v256();
        hasher.update(signature.as_bytes());
        let mut output = [0u8; 32];
        hasher.finalize(&mut output);
        output.to_vec()
    }

    /// An `int24` tick as an indexed event topic.
    fn tick_topic(tick: i32) -> Vec<u8> {
        let mut word = if tick < 0 { [0xffu8; 32] } else { [0u8; 32] };
        word[28..].copy_from_slice(&tick.to_be_bytes());
        word.to_vec()
    }

    fn mint_log(emitter: &[u8; 20], bottom: Vec<u8>, top: Vec<u8>) -> Log {
        Log {
            address: emitter.to_vec(),
            topics: vec![
                topic("Mint(address,address,int24,int24,uint128,uint256,uint256)"),
                vec![0u8; 32],
                bottom,
                top,
            ],
            data: vec![0u8; 128],
            ..Default::default()
        }
    }

    fn burn_log(emitter: &[u8; 20], bottom: Vec<u8>, top: Vec<u8>) -> Log {
        Log {
            address: emitter.to_vec(),
            topics: vec![
                topic("Burn(address,int24,int24,uint128,uint256,uint256)"),
                vec![0u8; 32],
                bottom,
                top,
            ],
            data: vec![0u8; 96],
            ..Default::default()
        }
    }

    /// A timepoint's first word: initialized, written at `timestamp`.
    fn timepoint_first_word(timestamp: u32) -> Word {
        let mut word = [0u8; 32];
        word[31] = 1;
        word[27..31].copy_from_slice(&timestamp.to_be_bytes());
        word
    }

    #[test]
    fn ticks_touched_decode_only_the_pools_own_logs() {
        let tx = tx_with(vec![call_with(
            vec![],
            vec![
                mint_log(&POOL, tick_topic(-60), tick_topic(60)),
                // Another contract's log of the same shape, with ticks no int24 holds.
                mint_log(&OTHER, vec![0x7f; 32], vec![0x7f; 32]),
                burn_log(&POOL, tick_topic(-887_220), tick_topic(120)),
            ],
        )]);

        assert_eq!(ticks_touched(&tx, &POOL), vec![-60, 60, -887_220, 120]);
        assert!(ticks_touched(&tx, &OPERATOR).is_empty());
    }

    #[test]
    fn ticks_of_reverted_calls_are_not_touched() {
        let mut reverted =
            call_with(vec![], vec![mint_log(&POOL, tick_topic(-60), tick_topic(60))]);
        reverted.state_reverted = true;

        assert!(ticks_touched(&tx_with(vec![reverted]), &POOL).is_empty());
    }

    #[test]
    fn word_changes_keep_the_first_old_and_the_last_new_value() {
        let key = slot_word(GLOBAL_STATE_SLOT);
        let mut reverted =
            call_with(vec![change(&OPERATOR, key, word_with(0), word_with(9), 1)], vec![]);
        reverted.state_reverted = true;
        let tx = tx_with(vec![
            call_with(
                vec![
                    change(&POOL, key, word_with(1), word_with(2), 7),
                    change(&POOL, key, word_with(2), word_with(3), 9),
                ],
                vec![],
            ),
            // An earlier write recorded in a later call: ordinals, not call order, decide.
            call_with(vec![change(&POOL, key, word_with(0), word_with(1), 3)], vec![]),
            reverted,
        ]);

        let changes = word_changes(&tx).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[&POOL.to_vec()][&key], (word_with(0), word_with(3)));
    }

    #[test]
    fn word_changes_reject_a_value_that_is_not_a_word() {
        let mut short = change(&POOL, slot_word(GLOBAL_STATE_SLOT), word_with(0), word_with(1), 1);
        short.new_value.pop();

        assert!(word_changes(&tx_with(vec![call_with(vec![short], vec![])])).is_err());
    }

    #[test]
    fn operator_attributes_complete_a_timepoint_from_the_store() {
        // The transaction rewrote only the first word of timepoint 5; the second comes from
        // the store.
        let (first_slot, second_slot) = timepoint_slots(5);
        let first = timepoint_first_word(1_000);
        let second = [9u8; 32];
        let keyed = BTreeMap::from([(slot_word(first_slot), ([0u8; 32], first))]);

        let attributes =
            operator_attributes(&keyed, |slot| (slot == second_slot).then_some(second));

        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "timepoints/5");
        assert_eq!(attributes[0].change, i32::from(ChangeType::Creation));
        assert_eq!(&attributes[0].value[..32], &first);
        assert_eq!(&attributes[0].value[32..], &second);
    }

    #[test]
    fn operator_attributes_prune_the_timepoints_the_fee_cannot_reach() {
        // Ring indices 0..=99 were written 1000 seconds apart; index 100 is written now, so
        // the window start falls between 13 and 14 and the previous write's 12 goes.
        let timestamp = |index: u16| 1_000_000 + 1_000 * u32::from(index);
        let (first_slot, second_slot) = timepoint_slots(100);
        let keyed = BTreeMap::from([
            (slot_word(first_slot), ([0u8; 32], timepoint_first_word(timestamp(100)))),
            (slot_word(second_slot), ([0u8; 32], [1u8; 32])),
        ]);
        let stored = |slot: u64| {
            timepoint_of_slot(slot)
                .filter(|(index, _)| *index < 100)
                .map(
                    |(index, second)| {
                        if second {
                            [1u8; 32]
                        } else {
                            timepoint_first_word(timestamp(index))
                        }
                    },
                )
        };

        let attributes = operator_attributes(&keyed, stored);

        let changes: Vec<_> = attributes
            .iter()
            .map(|a| (a.name.as_str(), a.change, a.value.len()))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("timepoints/100", i32::from(ChangeType::Creation), 64),
                ("timepoints/12", i32::from(ChangeType::Deletion), 0)
            ]
        );
    }

    #[test]
    fn operator_attributes_emit_the_fee_configurations() {
        let keyed = BTreeMap::from([
            (slot_word(FEE_CONFIG_ZTO_SLOT), ([0u8; 32], [7u8; 32])),
            (slot_word(FEE_CONFIG_OTZ_SLOT), ([3u8; 32], [8u8; 32])),
        ]);

        let attributes = operator_attributes(&keyed, |_| None);

        let changes: Vec<_> = attributes
            .iter()
            .map(|a| (a.name.as_str(), a.change, a.value[0]))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("fee_config_zto", i32::from(ChangeType::Creation), 7),
                ("fee_config_otz", i32::from(ChangeType::Update), 8)
            ]
        );
    }

    #[test]
    fn pool_attributes_cover_the_ticks_a_position_change_touched() {
        let mut liquidity = [0u8; 32];
        liquidity[31] = 5;
        let mut referenced = [0u8; 32];
        referenced[31] = 1; // liquidityTotal 1, liquidityDelta 0
        let keyed = BTreeMap::from([
            (slot_word(LIQUIDITY_SLOT), ([0u8; 32], liquidity)),
            (tick_slot(60), ([0u8; 32], referenced)),
            (tick_slot(120), (referenced, [0u8; 32])),
            (tick_slot(180), ([0u8; 32], referenced)),
        ]);

        // Tick 180 changed in storage but no Mint or Burn named it.
        let attributes = pool_attributes(&keyed, &[60, 120, 60]);

        let changes: Vec<_> = attributes
            .iter()
            .map(|a| (a.name.as_str(), a.change))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("liquidity", i32::from(ChangeType::Update)),
                ("ticks/60", i32::from(ChangeType::Creation)),
                ("ticks/120", i32::from(ChangeType::Deletion))
            ]
        );
    }

    #[test]
    fn a_created_pool_starts_from_zero_and_takes_the_constructors_fees() {
        // The constructor writes `globalState` with `feeZto` and `feeOtz` at `BASE_FEE`; the
        // factory writes the operator's zero-to-one fee configuration.
        let mut constructor = [0u8; 32];
        constructor[32 - 23 - 2..32 - 23].copy_from_slice(&100u16.to_be_bytes());
        constructor[32 - 25 - 2..32 - 25].copy_from_slice(&100u16.to_be_bytes());
        let tx = tx_with(vec![call_with(
            vec![
                change(&POOL, slot_word(GLOBAL_STATE_SLOT), [0u8; 32], constructor, 1),
                change(&OPERATOR, slot_word(FEE_CONFIG_ZTO_SLOT), [0u8; 32], [7u8; 32], 2),
                change(&OTHER, slot_word(GLOBAL_STATE_SLOT), [0u8; 32], [1u8; 32], 3),
            ],
            vec![],
        )]);

        let per_component =
            transaction_attributes(&tx, &[component()], component_at, |_: &[u8], _: u64| None)
                .unwrap();

        assert_eq!(per_component.len(), 1, "untracked contracts emit nothing");
        let attributes = &per_component[&POOL.to_hex()];
        let value = |name: &str| {
            attributes
                .iter()
                .find(|a| a.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .value
                .clone()
        };
        assert_eq!(value("fee_zto"), 100u16.to_be_bytes());
        assert_eq!(value("fee_otz"), 100u16.to_be_bytes());
        assert_eq!(value("liquidity"), vec![0u8; 16]);
        assert_eq!(value("sqrt_price_x96"), vec![0u8; 20]);
        assert_eq!(value("tick"), vec![0u8; 3]);
        assert_eq!(value("timepoint_index"), vec![0u8; 2]);
        assert_eq!(value("volume_per_liquidity_in_block"), vec![0u8; 16]);
        assert_eq!(value("fee_config_zto"), vec![7u8; 32]);
        assert_eq!(attributes.len(), 8);
        assert!(attributes
            .iter()
            .all(|a| a.change == i32::from(ChangeType::Creation)));
    }

    #[test]
    fn a_created_pool_without_storage_writes_still_gets_its_initial_attributes() {
        let per_component =
            transaction_attributes(&tx_with(vec![]), &[component()], component_at, |_, _| None)
                .unwrap();

        assert_eq!(per_component[&POOL.to_hex()].len(), initial_attributes().len());
    }

    #[test]
    fn an_existing_pools_writes_are_updates() {
        let mut liquidity = [0u8; 32];
        liquidity[31] = 5;
        let tx = tx_with(vec![call_with(
            vec![change(&POOL, slot_word(LIQUIDITY_SLOT), [0u8; 32], liquidity, 1)],
            vec![],
        )]);

        let per_component =
            transaction_attributes(&tx, &[], component_at, |_: &[u8], _: u64| None).unwrap();

        let attributes = &per_component[&POOL.to_hex()];
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "liquidity");
        assert_eq!(attributes[0].change, i32::from(ChangeType::Update));
    }
}
