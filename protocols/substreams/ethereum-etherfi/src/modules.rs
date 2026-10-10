//! EtherFi indexing: the LiquidityPool venue and the weETH wrapper as two components.
//!
//! Neither contract has a creation event to discover, so the manifest carries a storage snapshot
//! in `params`, along with the transaction in `start_block` to anchor the components to. Every
//! later block is driven by raw storage writes on four contracts, with upgrade guards on all five
//! proxies whose code the integration relies on.
//!
//! Handlers below are in manifest order.

use anyhow::{anyhow, Result};
use itertools::Itertools;
use std::{cell::LazyCell, collections::HashMap};
use substreams::{pb::substreams::StoreDeltas, prelude::*, scalar::BigInt};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    models::{
        BlockChanges, ChangeType, EntityChanges, ImplementationType, ProtocolComponent,
        TransactionChangesBuilder,
    },
    prelude::{BalanceChange, BlockTransactionProtocolComponents, TransactionProtocolComponents},
};

use crate::{
    constants::{
        Component, TrackedSlot, EETH_ADDRESS, ETH_ADDRESS, LIQUIDITY_POOL_VALUE_KEY,
        TOTAL_SHARES_KEY, TRACKED_SLOTS, WEETH_ADDRESS, WEETH_SHARES_KEY,
    },
    state::{unpack_fields, BalanceState, InitialState},
    upgrades::detect_upgrades,
    utils::{bytes_from_hex, ordered_storage_changes},
};

/// Creates both components on `start_block`, and nothing on any other block.
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let initial_state = InitialState::parse(&params)?;

    if block.number != initial_state.start_block {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] });
    }

    let creation_tx = bytes_from_hex(&initial_state.creation_tx)?;
    let tx = block
        .transactions()
        .find(|tx| tx.hash == creation_tx)
        .ok_or_else(|| {
            anyhow!(
                "Activation transaction {} not found in block {}",
                initial_state.creation_tx,
                block.number
            )
        })?;

    Ok(BlockTransactionProtocolComponents {
        tx_components: vec![TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: vec![pool_component(), wrapper_component()],
        }],
    })
}

/// ETH -> eETH through `LiquidityPool.deposit`, eETH -> ETH through
/// `EtherFiRedemptionManager.redeemEEth`.
fn pool_component() -> ProtocolComponent {
    ProtocolComponent::new(Component::Pool.id())
        .with_tokens(&[EETH_ADDRESS, ETH_ADDRESS])
        .as_swap_type("ethereum_etherfi_pool", ImplementationType::Custom)
}

/// eETH <-> weETH through `wrap` / `unwrap`.
fn wrapper_component() -> ProtocolComponent {
    ProtocolComponent::new(Component::Wrapper.id())
        .with_tokens(&[WEETH_ADDRESS, EETH_ADDRESS])
        .as_swap_type("ethereum_etherfi_pool", ImplementationType::Custom)
}

/// Carries the latest raw value of every slot a component balance is derived from, so a block
/// that touches only one of them can still report both balances. Seeded from the manifest
/// snapshot on `start_block`.
#[substreams::handlers::store]
pub fn store_balance_slots(params: String, block: eth::v2::Block, store: StoreSetBigInt) {
    let initial_state = InitialState::parse(&params).expect("Failed to parse EtherFi params");

    if block.number == initial_state.start_block {
        let seed = initial_state
            .balance_state()
            .expect("Failed to decode the EtherFi initial state");
        store.set(0, LIQUIDITY_POOL_VALUE_KEY, &seed.liquidity_pool_value);
        store.set(0, TOTAL_SHARES_KEY, &seed.total_shares);
        store.set(0, WEETH_SHARES_KEY, &seed.weeth_shares);
        return;
    }

    for (ordinal, key, value) in balance_store_writes(&block) {
        store.set(ordinal, key, &value);
    }
}

/// Every write `block` calls for on the balance store: each tracked slot that feeds a balance,
/// under the key it is stored as, with the word the block left there.
fn balance_store_writes(block: &eth::v2::Block) -> Vec<(u64, &'static str, BigInt)> {
    let mut writes = Vec::new();
    for tx in block.transactions() {
        let feeds_a_balance = |change: &eth::v2::StorageChange| {
            tracked_slot(&change.address, &change.key)
                .and_then(|slot| slot.balance_key)
                .is_some()
        };
        for storage_change in ordered_storage_changes(tx, feeds_a_balance) {
            let Some(key) = tracked_slot(&storage_change.address, &storage_change.key)
                .and_then(|slot| slot.balance_key)
            else {
                continue;
            };
            writes.push((
                storage_change.ordinal,
                key,
                BigInt::from_unsigned_bytes_be(&storage_change.new_value),
            ));
        }
    }
    writes
}

/// The tracked slot at `position` on `contract`, or `None` for a write this package ignores.
///
/// Both halves of the key matter: the LiquidityPool and eETH are separate contracts whose
/// low-numbered slots overlap, and the LiquidityPool's slot 202 holds an address.
fn tracked_slot(contract: &[u8], position: &[u8]) -> Option<&'static TrackedSlot> {
    TRACKED_SLOTS
        .iter()
        .find(|slot| contract == slot.contract && position == slot.position)
}

/// Emits the component creations on `start_block`, and attribute plus balance updates on every
/// later block. The two paths are mutually exclusive.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    protocol_components: BlockTransactionProtocolComponents,
    balance_deltas: StoreDeltas,
    balance_store: StoreGetBigInt,
) -> Result<BlockChanges> {
    let initial_state = InitialState::parse(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    if block.number == initial_state.start_block {
        initialize_protocol_components(
            &initial_state,
            protocol_components,
            &mut transaction_changes,
        )?;
    } else {
        handle_state_updates(&block, &balance_deltas, &balance_store, &mut transaction_changes);
    }
    // Every block, the snapshot block included: an implementation installed there that is not
    // the recorded one means the snapshot itself was taken against the wrong layout.
    pause_on_upgrade(&block, &initial_state, &mut transaction_changes)?;

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: vec![],
    })
}

/// Registers both components on the activation transaction and seeds them from the snapshot.
fn initialize_protocol_components(
    initial_state: &InitialState,
    protocol_components: BlockTransactionProtocolComponents,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    let tx_component = protocol_components
        .tx_components
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Missing activation transaction component"))?;
    let tx = tx_component
        .tx
        .as_ref()
        .ok_or_else(|| anyhow!("Activation transaction missing"))?;

    let builder = transaction_changes
        .entry(tx.index)
        .or_insert_with(|| TransactionChangesBuilder::new(tx));

    for component in tx_component.components {
        builder.add_protocol_component(&component);
    }

    for component in [Component::Pool, Component::Wrapper] {
        builder.add_entity_change(&EntityChanges {
            component_id: component.id().to_string(),
            attributes: initial_state.creation_attributes(component)?,
        });
    }

    let balances = initial_state.balance_state()?;
    add_pool_balance(builder, &balances);
    add_wrapper_balance(builder, &balances);

    Ok(())
}

/// Pauses both components on the transaction that puts a tracked proxy behind an implementation
/// other than the one the snapshot was taken against.
///
/// The slots this package reads were verified for those implementations only. Attribute and
/// balance updates keep flowing while paused, so components un-paused after the layout is
/// re-verified are current; a layout that did move needs a new snapshot.
fn pause_on_upgrade(
    block: &eth::v2::Block,
    initial_state: &InitialState,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    for tx in detect_upgrades(block, initial_state)? {
        let builder = transaction_changes
            .entry(tx.index as u64)
            .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));
        for component in [Component::Pool, Component::Wrapper] {
            builder.change_component_pause_state(component.id(), true);
        }
    }
    Ok(())
}

/// Turns storage writes on the tracked contracts into per-transaction attribute and balance
/// changes.
fn handle_state_updates(
    block: &eth::v2::Block,
    balance_deltas: &StoreDeltas,
    balance_store: &StoreGetBigInt,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    // Deferred: most blocks touch none of the three balance slots and never read this.
    let mut balances = LazyCell::new(|| block_start_balance_state(balance_deltas, balance_store));

    for tx in block.transactions() {
        let mut pool_balance_touched = false;
        let mut wrapper_balance_touched = false;

        let is_tracked =
            |change: &eth::v2::StorageChange| tracked_slot(&change.address, &change.key).is_some();
        for storage_change in ordered_storage_changes(tx, is_tracked) {
            let Some(tracked) = tracked_slot(&storage_change.address, &storage_change.key) else {
                continue;
            };

            let builder = transaction_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

            for component in tracked.components {
                builder.add_entity_change(&EntityChanges {
                    component_id: component.id().to_string(),
                    attributes: unpack_fields(
                        tracked,
                        &storage_change.new_value,
                        ChangeType::Update,
                    ),
                });
            }

            if let Some(key) = tracked.balance_key {
                balances.apply(key, BigInt::from_unsigned_bytes_be(&storage_change.new_value));
                // The pool balance is the value word alone; the wrapper's needs all three.
                pool_balance_touched |= key == LIQUIDITY_POOL_VALUE_KEY;
                wrapper_balance_touched = true;
            }
        }

        // Balances are absolute, so one report per transaction that moved any of the inputs is
        // enough - intermediate values within the transaction are never observable.
        if pool_balance_touched || wrapper_balance_touched {
            let builder = transaction_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));
            if pool_balance_touched {
                add_pool_balance(builder, &balances);
            }
            if wrapper_balance_touched {
                add_wrapper_balance(builder, &balances);
            }
        }
    }
}

/// The pool's balance is `totalValueInLp`, the ETH redemptions are paid from.
fn add_pool_balance(builder: &mut TransactionChangesBuilder, balances: &BalanceState) {
    builder.add_balance_change(&BalanceChange {
        token: ETH_ADDRESS.to_vec(),
        balance: balances
            .pool_eth_balance()
            .to_signed_bytes_be(),
        component_id: Component::Pool.id().as_bytes().to_vec(),
    });
}

/// The wrapper's balance is `eETH.balanceOf(weETH)`: what unwrapping everything would pay out.
fn add_wrapper_balance(builder: &mut TransactionChangesBuilder, balances: &BalanceState) {
    builder.add_balance_change(&BalanceChange {
        token: EETH_ADDRESS.to_vec(),
        balance: balances
            .wrapper_eeth_balance()
            .to_signed_bytes_be(),
        component_id: Component::Wrapper
            .id()
            .as_bytes()
            .to_vec(),
    });
}

/// Rebuilds the balance inputs as of the start of the block.
///
/// The store module runs before this one, so `get_last` already reflects this block's writes.
/// Where a key changed in this block, the first delta's `old_value` is the value it held on
/// entry; otherwise the store still holds it.
fn block_start_balance_state(
    balance_deltas: &StoreDeltas,
    balance_store: &StoreGetBigInt,
) -> BalanceState {
    let value_for = |key: &str| -> BigInt {
        match balance_deltas
            .deltas
            .iter()
            .filter(|delta| delta.key == key)
            .min_by_key(|delta| delta.ordinal)
        {
            Some(first_delta) => decode_store_value(key, &first_delta.old_value),
            None => balance_store
                .get_last(key)
                .unwrap_or_else(|| {
                    panic!("EtherFi store key {key} was never seeded; the store module runs first")
                }),
        }
    };

    BalanceState {
        liquidity_pool_value: value_for(LIQUIDITY_POOL_VALUE_KEY),
        total_shares: value_for(TOTAL_SHARES_KEY),
        weeth_shares: value_for(WEETH_SHARES_KEY),
    }
}

/// `StoreSetBigInt` serialises values as decimal strings.
///
/// A value that does not decode means the store module or the runtime is broken, so this panics
/// and names the key. The figure it returns is reported as a component balance.
///
/// An empty value is the seed on `start_block`: `StoreDeltas` carries an empty `old_value` for a
/// key's first write.
fn decode_store_value(key: &str, bytes: &[u8]) -> BigInt {
    if bytes.is_empty() {
        return BigInt::zero();
    }
    let text = std::str::from_utf8(bytes)
        .unwrap_or_else(|_| panic!("EtherFi store key {key} holds non-UTF-8 bytes: {bytes:02x?}"));
    text.parse::<BigInt>()
        .unwrap_or_else(|_| panic!("EtherFi store key {key} holds an unparsable value: {text:?}"))
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{
        Call, StorageChange, TransactionTrace, TransactionTraceStatus,
    };
    use tycho_substreams::models::Attribute;

    use super::*;
    use crate::{
        constants::{
            EETH_TOTAL_SHARES_POSITION, ETH_REDEMPTION_INFO_POSITION, ETH_REDEMPTION_INFO_SLOT,
            EXIT_FEE_BPS_ATTR, LIQUIDITY_POOL_ADDRESS, LIQUIDITY_POOL_VALUE_POSITION,
            REDEMPTION_MANAGER_ADDRESS,
        },
        upgrades::fixtures::{
            block_with, initial_state, rate_limiter_upgrade_to, OTHER, RATE_LIMITER_V1,
        },
    };

    /// A block whose only transaction succeeds and writes `key` on `address` in one call.
    fn block_with_storage_change(
        address: [u8; 20],
        key: [u8; 32],
        state_reverted: bool,
    ) -> eth::v2::Block {
        eth::v2::Block {
            number: 25_940_100,
            transaction_traces: vec![eth::v2::TransactionTrace {
                index: 3,
                status: TransactionTraceStatus::Succeeded as i32,
                calls: vec![eth::v2::Call {
                    storage_changes: vec![eth::v2::StorageChange {
                        address: address.to_vec(),
                        key: key.to_vec(),
                        new_value: vec![0x11u8; 32],
                        ..Default::default()
                    }],
                    state_reverted,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn updates_for(block: &eth::v2::Block) -> HashMap<u64, TransactionChangesBuilder> {
        let mut transaction_changes = HashMap::new();
        handle_state_updates(
            block,
            &StoreDeltas::default(),
            &StoreGetBigInt::new(0),
            &mut transaction_changes,
        );
        transaction_changes
    }

    /// A live write to a tracked slot is reported against the slot's components as an update.
    #[test]
    fn a_tracked_write_is_reported_as_an_update() {
        let block = block_with_storage_change(
            REDEMPTION_MANAGER_ADDRESS,
            ETH_REDEMPTION_INFO_POSITION,
            false,
        );

        let mut updates = updates_for(&block);

        let changes = updates
            .remove(&3)
            .expect("changes on the writing transaction")
            .build()
            .expect("the write is a change");
        let [entity] = changes.entity_changes.as_slice() else {
            panic!("expected one entity change, got {:?}", changes.entity_changes);
        };
        assert_eq!(entity.component_id, Component::Pool.id());
        assert_eq!(entity.attributes.len(), ETH_REDEMPTION_INFO_SLOT.fields.len());
        for attribute in &entity.attributes {
            assert_eq!(attribute.change, ChangeType::Update as i32);
        }
    }

    /// A call whose state was reverted wrote nothing the chain kept.
    #[test]
    fn a_reverted_call_reports_nothing() {
        let block = block_with_storage_change(
            REDEMPTION_MANAGER_ADDRESS,
            ETH_REDEMPTION_INFO_POSITION,
            true,
        );

        assert!(updates_for(&block).is_empty());
        assert!(balance_store_writes(&block).is_empty());
    }

    /// The balance store is what the reported balances are computed from, so each slot has to
    /// reach its own key, from the right contract, on a call the chain kept.
    #[test]
    fn the_balance_store_takes_one_write_per_slot_that_feeds_a_balance() {
        let fees = block_with_storage_change(
            REDEMPTION_MANAGER_ADDRESS,
            ETH_REDEMPTION_INFO_POSITION,
            false,
        );
        // The fee word is reported as attributes but moves no balance.
        assert!(balance_store_writes(&fees).is_empty());

        let shares = block_with_storage_change(EETH_ADDRESS, EETH_TOTAL_SHARES_POSITION, false);
        let writes = balance_store_writes(&shares);
        let [(_, key, value)] = writes.as_slice() else {
            panic!("expected one write, got {writes:?}");
        };
        assert_eq!(*key, TOTAL_SHARES_KEY);
        assert_eq!(*value, BigInt::from_unsigned_bytes_be(&[0x11u8; 32]));

        // The LiquidityPool holds an address at eETH's `totalShares` position.
        let elsewhere =
            block_with_storage_change(LIQUIDITY_POOL_ADDRESS, EETH_TOTAL_SHARES_POSITION, false);
        assert!(balance_store_writes(&elsewhere).is_empty());
    }

    /// The pause lands on the transaction that installed the other implementation, on both
    /// components, as the `paused` attribute with `PausingReason::Substreams` (1) as its value.
    #[test]
    fn an_upgrade_pauses_both_components_on_its_transaction() {
        let block = block_with(vec![rate_limiter_upgrade_to(OTHER)], false);
        let mut transaction_changes = HashMap::new();

        pause_on_upgrade(&block, &initial_state(), &mut transaction_changes).expect("pause");

        let changes = transaction_changes
            .remove(&7)
            .expect("changes on the upgrade's transaction")
            .build()
            .expect("the pause is a change");
        assert!(transaction_changes.is_empty(), "no other transaction changed");
        let paused = Attribute {
            name: "paused".to_string(),
            value: vec![1u8],
            change: ChangeType::Creation as i32,
        };
        let mut entities = changes.entity_changes;
        entities.sort_by(|a, b| a.component_id.cmp(&b.component_id));
        assert_eq!(
            entities,
            vec![
                EntityChanges {
                    component_id: Component::Pool.id().to_string(),
                    attributes: vec![paused.clone()],
                },
                EntityChanges {
                    component_id: Component::Wrapper.id().to_string(),
                    attributes: vec![paused],
                },
            ]
        );
    }

    #[test]
    fn a_weeth_upgrade_pauses_both_components() {
        let change = crate::upgrades::fixtures::upgrade_write(WEETH_ADDRESS, OTHER);
        let block = block_with(vec![change], false);
        let mut transaction_changes = HashMap::new();
        pause_on_upgrade(&block, &initial_state(), &mut transaction_changes).expect("pause");
        let changes = transaction_changes
            .remove(&7)
            .expect("weETH upgrade must pause the components")
            .build()
            .expect("pause changes");
        assert_eq!(changes.entity_changes.len(), 2);
        for component in [Component::Pool, Component::Wrapper] {
            let entity = changes
                .entity_changes
                .iter()
                .find(|entity| entity.component_id == component.id())
                .expect("component is paused");
            assert!(entity
                .attributes
                .iter()
                .any(|attribute| attribute.name == "paused" && attribute.value == vec![1]));
        }
    }

    #[test]
    fn writing_the_recorded_implementation_changes_nothing() {
        let block = block_with(vec![rate_limiter_upgrade_to(RATE_LIMITER_V1)], false);
        let mut transaction_changes = HashMap::new();

        pause_on_upgrade(&block, &initial_state(), &mut transaction_changes).expect("pause");

        assert!(transaction_changes.is_empty());
    }

    /// Each row is reachable by its own contract and position, so no two rows share a key and
    /// no attribute is paired with the wrong slot.
    #[test]
    fn every_tracked_slot_resolves_to_its_own_row() {
        for slot in TRACKED_SLOTS.iter() {
            let found =
                tracked_slot(&slot.contract, &slot.position).expect("declared slot resolves");
            let found_names: Vec<_> = found
                .fields
                .iter()
                .map(|f| f.attribute)
                .collect();
            let want_names: Vec<_> = slot
                .fields
                .iter()
                .map(|f| f.attribute)
                .collect();
            assert_eq!(found_names, want_names);
            assert_eq!(found.balance_key, slot.balance_key);
        }
    }

    /// The LiquidityPool's slot 202 holds an address, and eETH's slot 202 holds `totalShares`.
    /// A write to the former must not be read as the latter.
    #[test]
    fn the_same_position_on_another_contract_is_not_tracked() {
        assert!(tracked_slot(&LIQUIDITY_POOL_ADDRESS, &EETH_TOTAL_SHARES_POSITION).is_none());
        assert!(tracked_slot(&EETH_ADDRESS, &LIQUIDITY_POOL_VALUE_POSITION).is_none());
        assert!(tracked_slot(&REDEMPTION_MANAGER_ADDRESS, &LIQUIDITY_POOL_VALUE_POSITION).is_none());
    }

    #[test]
    fn untracked_positions_resolve_to_none() {
        assert!(tracked_slot(&LIQUIDITY_POOL_ADDRESS, &[0u8; 32]).is_none());
        assert!(tracked_slot(&[0u8; 20], &LIQUIDITY_POOL_VALUE_POSITION).is_none());
        assert!(tracked_slot(&[], &[]).is_none());
    }

    /// Every attribute name is unique across the whole table, so no two slots can report the
    /// same name and silently overwrite one another.
    #[test]
    fn attribute_names_are_unique() {
        let mut names: Vec<&str> = TRACKED_SLOTS
            .iter()
            .flat_map(|slot| slot.fields.iter().map(|f| f.attribute))
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate attribute name in TRACKED_SLOTS");
        assert_eq!(total, 19);
    }

    /// Each slot's fields tile its word from the bottom without overlapping, so no bit is
    /// reported twice or dropped between two attributes.
    #[test]
    fn packed_fields_tile_their_word() {
        for slot in TRACKED_SLOTS.iter() {
            let mut next = 0u32;
            for field in slot.fields {
                assert_eq!(field.offset, next, "gap or overlap before {}", field.attribute);
                next = field.offset + field.width;
            }
            assert!(next <= 256, "slot overflows a word at {}", slot.fields[0].attribute);
        }
    }

    /// Every slot reaches at least one component, and the balance inputs reach the component
    /// whose balance they feed.
    #[test]
    fn every_slot_reaches_a_component() {
        for slot in TRACKED_SLOTS.iter() {
            assert!(
                !slot.components.is_empty(),
                "{} reaches no component",
                slot.fields[0].attribute
            );
        }
        let wrapper_inputs: Vec<_> = TRACKED_SLOTS
            .iter()
            .filter(|slot| slot.balance_key.is_some())
            .map(|slot| {
                slot.components
                    .contains(&Component::Wrapper)
            })
            .collect();
        assert_eq!(wrapper_inputs, [true, true, true]);
    }

    /// The three inputs `BalanceState` derives both balances from, and only those.
    #[test]
    fn only_the_balance_inputs_carry_a_store_key() {
        let keys: Vec<_> = TRACKED_SLOTS
            .iter()
            .filter_map(|slot| slot.balance_key)
            .collect();

        assert_eq!(keys, [LIQUIDITY_POOL_VALUE_KEY, TOTAL_SHARES_KEY, WEETH_SHARES_KEY]);
    }

    /// A key's first delta carries an empty `old_value`, which means "nothing yet".
    #[test]
    fn an_empty_store_value_decodes_to_zero() {
        assert_eq!(decode_store_value(TOTAL_SHARES_KEY, &[]), BigInt::zero());
    }

    #[test]
    fn a_decimal_store_value_round_trips() {
        assert_eq!(
            decode_store_value(TOTAL_SHARES_KEY, b"2001243491556134113932753"),
            "2001243491556134113932753"
                .parse::<BigInt>()
                .unwrap()
        );
    }

    /// Zero is a plausible balance, so decoding has to fail loudly.
    #[test]
    #[should_panic(expected = "unparsable value")]
    fn an_unparsable_store_value_panics_rather_than_reading_as_zero() {
        decode_store_value(TOTAL_SHARES_KEY, b"not a number");
    }

    #[test]
    #[should_panic(expected = "non-UTF-8")]
    fn a_non_utf8_store_value_panics() {
        decode_store_value(TOTAL_SHARES_KEY, &[0xff, 0xfe]);
    }

    /// One transaction whose parent call writes `key` before and after a child call. The trace
    /// lists the parent's two writes together, so ordinal order is the only execution order.
    fn nested_writes(address: [u8; 20], key: [u8; 32]) -> eth::v2::Block {
        let write = |ordinal, value| StorageChange {
            address: address.to_vec(),
            key: key.to_vec(),
            ordinal,
            new_value: vec![value; 32],
            ..Default::default()
        };
        eth::v2::Block {
            transaction_traces: vec![TransactionTrace {
                index: 3,
                status: TransactionTraceStatus::Succeeded as i32,
                calls: vec![
                    Call {
                        index: 1,
                        storage_changes: vec![write(10, 1), write(30, 3)],
                        ..Default::default()
                    },
                    Call {
                        index: 2,
                        parent_index: 1,
                        storage_changes: vec![write(20, 2)],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The attribute a transaction leaves is the value of its last write in execution order,
    /// which is the parent's second write, not the child's.
    #[test]
    fn nested_calls_emit_the_last_executed_attribute_write() {
        let block = nested_writes(REDEMPTION_MANAGER_ADDRESS, ETH_REDEMPTION_INFO_POSITION);

        let mut updates = updates_for(&block);

        let changes = updates
            .remove(&3)
            .expect("changes on the writing transaction")
            .build()
            .expect("the writes are a change");
        let fee = changes.entity_changes[0]
            .attributes
            .iter()
            .find(|attribute| attribute.name == EXIT_FEE_BPS_ATTR)
            .expect("the fee attribute");
        assert_eq!(BigInt::from_unsigned_bytes_be(&fee.value), BigInt::from(0x0303u32));
    }

    /// The store replays writes in the order they are set, so the balance store has to receive
    /// them in execution order for the last one to win.
    #[test]
    fn nested_calls_write_balance_store_in_execution_order() {
        let block = nested_writes(EETH_ADDRESS, EETH_TOTAL_SHARES_POSITION);

        let ordinals: Vec<u64> = balance_store_writes(&block)
            .iter()
            .map(|(ordinal, _, _)| *ordinal)
            .collect();

        assert_eq!(ordinals, vec![10, 20, 30]);
    }
}
