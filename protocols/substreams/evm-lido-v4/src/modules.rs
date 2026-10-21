//! Lido V4 indexing: one component covering the stETH staking pool and the wstETH wrapper.
//!
//! Neither contract has a creation event to discover, so the manifest carries a storage snapshot
//! in `params`, along with the transaction in `start_block` to anchor the component to. Every
//! later block is driven by raw stETH storage writes.
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
        TrackedSlot, BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY,
        CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY, ETH_ADDRESS, STETH_ADDRESS,
        STETH_COMPONENT_ID, TOTAL_AND_EXTERNAL_SHARES_KEY, TRACKED_SLOTS, WSTETH_ADDRESS,
    },
    state::{unpack_fields, BalanceState, InitialState},
    upgrades::detect_upgrades,
    utils::{bytes_from_hex, ordered_storage_changes},
};

/// Creates the component on `start_block`, and nothing on any other block.
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
            components: vec![create_component()],
        }],
    })
}

/// One component serves ETH -> stETH, stETH <-> wstETH and ETH -> wstETH
/// using the pool's shared rate and staking capacity.
fn create_component() -> ProtocolComponent {
    ProtocolComponent::new(STETH_COMPONENT_ID)
        .with_tokens(&[ETH_ADDRESS, STETH_ADDRESS, WSTETH_ADDRESS])
        .as_swap_type("lido_v4_pool", ImplementationType::Custom)
}

/// Carries the latest raw value of every slot that feeds the component balance, so a block that
/// touches only one of them can still report it. Seeded from the manifest snapshot on
/// `start_block`.
#[substreams::handlers::store]
pub fn store_balance_slots(params: String, block: eth::v2::Block, store: StoreSetBigInt) {
    let initial_state = InitialState::parse(&params).expect("Failed to parse Lido V4 params");

    if block.number == initial_state.start_block {
        let seed = initial_state
            .balance_state()
            .expect("Failed to decode the Lido V4 initial state");
        store.set(0, TOTAL_AND_EXTERNAL_SHARES_KEY, &seed.total_and_external_shares);
        store.set(
            0,
            BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY,
            &seed.buffered_ether_and_deposited_post_report,
        );
        store.set(
            0,
            CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY,
            &seed.cl_validators_balance_and_cl_pending_balance,
        );
        return;
    }

    for (ordinal, key, value) in balance_store_writes(&block) {
        store.set(ordinal, key, &value);
    }
}

/// Every write `block` calls for on the balance store: each stETH slot that feeds a balance,
/// under the key it is stored as, with the word the block left there.
fn balance_store_writes(block: &eth::v2::Block) -> Vec<(u64, &'static str, BigInt)> {
    let mut writes = Vec::new();
    for tx in block.transactions() {
        for storage_change in ordered_storage_changes(tx) {
            if let Some(key) = tracked_slot(&storage_change.key).and_then(|slot| slot.balance_key) {
                writes.push((
                    storage_change.ordinal,
                    key,
                    BigInt::from_unsigned_bytes_be(&storage_change.new_value),
                ));
            }
        }
    }
    writes
}

/// The tracked slot at `position`, or `None` for a slot this package ignores.
fn tracked_slot(position: &[u8]) -> Option<&'static TrackedSlot> {
    TRACKED_SLOTS
        .iter()
        .find(|slot| position == slot.position)
}

/// Emits the component creation on `start_block`, and attribute plus balance updates on every
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

/// Registers the component on the activation transaction and seeds it from the manifest snapshot.
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

    builder.add_entity_change(&EntityChanges {
        component_id: STETH_COMPONENT_ID.to_string(),
        attributes: initial_state.creation_attributes()?,
    });

    add_balance_changes(builder, &initial_state.balance_state()?);

    Ok(())
}

/// Turns stETH storage writes into per-transaction attribute and balance changes.
fn handle_state_updates(
    block: &eth::v2::Block,
    balance_deltas: &StoreDeltas,
    balance_store: &StoreGetBigInt,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    // Deferred: only three slots feed the balances and the consensus-layer one moves about once
    // a day, so most blocks touch none of them and never read this.
    let mut balances = LazyCell::new(|| block_start_balance_state(balance_deltas, balance_store));

    for tx in block.transactions() {
        let mut balance_slot_touched = false;

        for storage_change in ordered_storage_changes(tx) {
            let Some(tracked) = tracked_slot(&storage_change.key) else {
                continue;
            };

            let builder = transaction_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

            builder.add_entity_change(&EntityChanges {
                component_id: STETH_COMPONENT_ID.to_string(),
                attributes: unpack_fields(tracked, &storage_change.new_value, ChangeType::Update),
            });

            if let Some(key) = tracked.balance_key {
                let value = BigInt::from_unsigned_bytes_be(&storage_change.new_value);
                balances.apply(key, value);
                balance_slot_touched = true;
            }
        }

        // Balances are absolute, so one report per transaction that moved any of the inputs is
        // enough - intermediate values within the transaction are never observable.
        if balance_slot_touched {
            let builder = transaction_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));
            add_balance_changes(builder, &balances);
        }
    }
}

/// Pauses the component on the transaction that puts a tracked proxy behind an implementation
/// other than the one the snapshot was taken against.
///
/// The slots this package reads were verified for that implementation only. Attribute and
/// balance updates keep flowing while paused, so a component un-paused after the layout is
/// re-verified is current; a layout that did move needs a new snapshot.
fn pause_on_upgrade(
    block: &eth::v2::Block,
    initial_state: &InitialState,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    for tx in detect_upgrades(block, initial_state)? {
        let builder = transaction_changes
            .entry(tx.index as u64)
            .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));
        builder.change_component_pause_state(STETH_COMPONENT_ID, true);
    }
    Ok(())
}

/// Reports the component's absolute balance: `getTotalPooledEther()` in ETH.
///
/// That single figure is the whole protocol. The stETH the wrapper holds is already inside it, so
/// reporting it as well would count the same ether twice.
fn add_balance_changes(builder: &mut TransactionChangesBuilder, balances: &BalanceState) {
    builder.add_balance_change(&BalanceChange {
        token: ETH_ADDRESS.to_vec(),
        balance: balances
            .total_pooled_ether()
            .to_signed_bytes_be(),
        component_id: STETH_COMPONENT_ID.as_bytes().to_vec(),
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
                    panic!("Lido V4 store key {key} was never seeded; the store module runs first")
                }),
        }
    };

    BalanceState {
        total_and_external_shares: value_for(TOTAL_AND_EXTERNAL_SHARES_KEY),
        buffered_ether_and_deposited_post_report: value_for(
            BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY,
        ),
        cl_validators_balance_and_cl_pending_balance: value_for(
            CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY,
        ),
    }
}

/// `StoreSetBigInt` serialises values as decimal strings.
///
/// A value that does not decode means the store module or the runtime is broken, so this panics
/// and names the key. The figure it returns is reported as the component balance.
///
/// An empty value is the seed on `start_block`: `StoreDeltas` carries an empty `old_value` for a
/// key's first write.
fn decode_store_value(key: &str, bytes: &[u8]) -> BigInt {
    if bytes.is_empty() {
        return BigInt::zero();
    }
    let text = std::str::from_utf8(bytes)
        .unwrap_or_else(|_| panic!("Lido V4 store key {key} holds non-UTF-8 bytes: {bytes:02x?}"));
    text.parse::<BigInt>()
        .unwrap_or_else(|_| panic!("Lido V4 store key {key} holds an unparsable value: {text:?}"))
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::TransactionTraceStatus;
    use tycho_substreams::models::Attribute;

    use super::*;
    use crate::{
        constants::{
            ARAGON_APP_BASES_NAMESPACE, LIDO_KERNEL_ADDRESS, STAKING_STATE_POSITION,
            STAKING_STATE_SLOT, STETH_APP_ID, TOTAL_AND_EXTERNAL_SHARES_POSITION,
        },
        upgrades::fixtures::{block_with, initial_state, set_app, OTHER, V4},
    };

    /// A block whose only transaction succeeds and writes `key` on `address` in one call.
    fn block_with_storage_change(
        address: [u8; 20],
        key: [u8; 32],
        state_reverted: bool,
    ) -> eth::v2::Block {
        eth::v2::Block {
            number: 25_603_400,
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

    fn nested_writes(position: [u8; 32]) -> eth::v2::Block {
        let mut block = block_with_storage_change(STETH_ADDRESS, position, false);
        let write = |ordinal, value| eth::v2::StorageChange {
            address: STETH_ADDRESS.to_vec(),
            key: position.to_vec(),
            new_value: vec![value],
            ordinal,
            ..Default::default()
        };
        block.transaction_traces[0].calls = vec![
            eth::v2::Call {
                storage_changes: vec![write(10, 10), write(30, 30)],
                ..Default::default()
            },
            eth::v2::Call { storage_changes: vec![write(20, 20)], ..Default::default() },
        ];
        block
    }

    #[test]
    fn nested_calls_report_the_final_attribute_write() {
        let block = nested_writes(STAKING_STATE_POSITION);
        let changes = updates_for(&block)
            .remove(&3)
            .unwrap()
            .build()
            .unwrap();
        let attribute = changes.entity_changes[0]
            .attributes
            .iter()
            .find(|attribute| attribute.name == "prev_stake_block_number")
            .unwrap();
        assert_eq!(attribute.value, vec![30]);
    }

    #[test]
    fn nested_calls_write_balance_store_in_execution_order() {
        let writes = balance_store_writes(&nested_writes(TOTAL_AND_EXTERNAL_SHARES_POSITION));
        let ordinals: Vec<_> = writes
            .iter()
            .map(|(ordinal, _, _)| *ordinal)
            .collect();
        assert_eq!(ordinals, vec![10, 20, 30]);
        assert_eq!(writes.last().unwrap().2, BigInt::from(30));
    }

    #[test]
    fn nested_calls_report_the_final_balance_write() {
        use substreams::pb::substreams::StoreDelta;
        let block =
            nested_writes(crate::constants::BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_POSITION);
        let deltas = StoreDeltas {
            deltas: vec![
                StoreDelta {
                    key: TOTAL_AND_EXTERNAL_SHARES_KEY.to_string(),
                    old_value: b"100".to_vec(),
                    ..Default::default()
                },
                StoreDelta {
                    key: BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY.to_string(),
                    old_value: b"0".to_vec(),
                    ..Default::default()
                },
                StoreDelta {
                    key: CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY.to_string(),
                    old_value: b"0".to_vec(),
                    ..Default::default()
                },
            ],
        };
        let mut updates = HashMap::new();
        handle_state_updates(&block, &deltas, &StoreGetBigInt::new(0), &mut updates);
        let changes = updates
            .remove(&3)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(changes.balance_changes[0].balance, BigInt::from(30).to_signed_bytes_be());
    }

    /// A live write to a tracked slot is reported against the component as an update.
    #[test]
    fn a_tracked_write_is_reported_as_an_update() {
        let block = block_with_storage_change(STETH_ADDRESS, STAKING_STATE_POSITION, false);

        let mut updates = updates_for(&block);

        let changes = updates
            .remove(&3)
            .expect("changes on the writing transaction")
            .build()
            .expect("the write is a change");
        let [entity] = changes.entity_changes.as_slice() else {
            panic!("expected one entity change, got {:?}", changes.entity_changes);
        };
        assert_eq!(entity.component_id, STETH_COMPONENT_ID);
        assert_eq!(entity.attributes.len(), STAKING_STATE_SLOT.fields.len());
        for attribute in &entity.attributes {
            assert_eq!(attribute.change, ChangeType::Update as i32);
        }
    }

    /// Slot positions are not unique across contracts: the same position on another contract
    /// holds something else entirely, and decoding it as stETH's would report a fabricated
    /// value for the component.
    #[test]
    fn a_tracked_position_on_another_contract_is_ignored() {
        let other = LIDO_KERNEL_ADDRESS;
        let block = block_with_storage_change(other, STAKING_STATE_POSITION, false);

        assert!(updates_for(&block).is_empty());
    }

    /// The balance store is what the reported balances are computed from, so each slot has to
    /// reach its own key, from the right contract, on a call the chain kept.
    #[test]
    fn the_balance_store_takes_one_write_per_slot_that_feeds_a_balance() {
        let staking = block_with_storage_change(STETH_ADDRESS, STAKING_STATE_POSITION, false);
        // The staking word is reported as attributes but moves no balance.
        assert!(balance_store_writes(&staking).is_empty());

        let shares =
            block_with_storage_change(STETH_ADDRESS, TOTAL_AND_EXTERNAL_SHARES_POSITION, false);
        let writes = balance_store_writes(&shares);
        let [(_, key, value)] = writes.as_slice() else {
            panic!("expected one write, got {writes:?}");
        };
        assert_eq!(*key, TOTAL_AND_EXTERNAL_SHARES_KEY);
        assert_eq!(*value, BigInt::from_unsigned_bytes_be(&[0x11u8; 32]));

        let elsewhere = block_with_storage_change(
            LIDO_KERNEL_ADDRESS,
            TOTAL_AND_EXTERNAL_SHARES_POSITION,
            false,
        );
        assert!(balance_store_writes(&elsewhere).is_empty());

        let reverted =
            block_with_storage_change(STETH_ADDRESS, TOTAL_AND_EXTERNAL_SHARES_POSITION, true);
        assert!(balance_store_writes(&reverted).is_empty());
    }

    /// A call whose state was reverted wrote nothing the chain kept.
    #[test]
    fn a_reverted_call_reports_nothing() {
        let block = block_with_storage_change(STETH_ADDRESS, STAKING_STATE_POSITION, true);

        assert!(updates_for(&block).is_empty());
    }

    /// The pause lands on the transaction that installed the other implementation, as the
    /// `paused` attribute with `PausingReason::Substreams` (1) as its value.
    #[test]
    fn an_upgrade_pauses_the_component_on_its_transaction() {
        let block = block_with(
            vec![set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, OTHER)],
            TransactionTraceStatus::Succeeded,
        );
        let mut transaction_changes = HashMap::new();

        pause_on_upgrade(&block, &initial_state(), &mut transaction_changes).expect("pause");

        let changes = transaction_changes
            .remove(&7)
            .expect("changes on the upgrade's transaction")
            .build()
            .expect("the pause is a change");
        assert!(transaction_changes.is_empty(), "no other transaction changed");
        let [entity] = changes.entity_changes.as_slice() else {
            panic!("expected one entity change, got {:?}", changes.entity_changes);
        };
        assert_eq!(entity.component_id, STETH_COMPONENT_ID);
        assert_eq!(
            entity.attributes,
            vec![Attribute {
                name: "paused".to_string(),
                value: vec![1u8],
                change: ChangeType::Creation as i32,
            }]
        );
    }

    #[test]
    fn installing_the_recorded_implementation_changes_nothing() {
        let block = block_with(
            vec![set_app(ARAGON_APP_BASES_NAMESPACE, STETH_APP_ID, V4)],
            TransactionTraceStatus::Succeeded,
        );
        let mut transaction_changes = HashMap::new();

        pause_on_upgrade(&block, &initial_state(), &mut transaction_changes).expect("pause");

        assert!(transaction_changes.is_empty());
    }

    /// Each row is reachable by its own position, so no two rows share one and no attribute is
    /// paired with the wrong slot.
    #[test]
    fn every_tracked_slot_resolves_to_its_own_row() {
        for slot in TRACKED_SLOTS.iter() {
            let found = tracked_slot(&slot.position).expect("declared slot resolves");
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

    #[test]
    fn untracked_positions_resolve_to_none() {
        assert!(tracked_slot(&[0u8; 32]).is_none());
        assert!(tracked_slot(&[]).is_none());
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
        assert_eq!(total, 11);
    }

    /// Each slot's fields tile its word without overlapping, so no bit is reported twice or
    /// dropped between two attributes.
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

    /// A key's first delta carries an empty `old_value`, which means "nothing yet", not a
    /// malformed store.
    #[test]
    fn an_empty_store_value_decodes_to_zero() {
        assert_eq!(decode_store_value(TOTAL_AND_EXTERNAL_SHARES_KEY, &[]), BigInt::zero());
    }

    #[test]
    fn a_decimal_store_value_round_trips() {
        assert_eq!(
            decode_store_value(TOTAL_AND_EXTERNAL_SHARES_KEY, b"7526667021904051320418763"),
            "7526667021904051320418763"
                .parse::<BigInt>()
                .unwrap()
        );
    }

    /// Zero is a plausible pooled ether, so a value that cannot be decoded must not become one.
    #[test]
    #[should_panic(expected = "unparsable value")]
    fn an_unparsable_store_value_panics_rather_than_reading_as_zero() {
        decode_store_value(TOTAL_AND_EXTERNAL_SHARES_KEY, b"not a number");
    }

    #[test]
    #[should_panic(expected = "non-UTF-8")]
    fn a_non_utf8_store_value_panics() {
        decode_store_value(TOTAL_AND_EXTERNAL_SHARES_KEY, &[0xff, 0xfe]);
    }

    /// The three inputs `BalanceState` reconstructs `totalPooledEther` from, and only those.
    #[test]
    fn only_the_pooled_ether_inputs_carry_a_store_key() {
        let keys: Vec<_> = TRACKED_SLOTS
            .iter()
            .filter_map(|slot| slot.balance_key)
            .collect();

        assert_eq!(
            keys,
            [
                TOTAL_AND_EXTERNAL_SHARES_KEY,
                BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY,
                CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY,
            ]
        );
    }
}
