//! Lido V3 indexing: the stETH staking pool and the wstETH wrapper.
//!
//! Neither contract has a creation event to discover, so the manifest carries a storage snapshot
//! in `params` and every later block is driven by raw stETH storage writes.
//!
//! Handlers below are in manifest order.

use anyhow::{anyhow, Result};
use itertools::Itertools;
use std::collections::HashMap;
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
        BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR, BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY,
        BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_POSITION, CL_BALANCE_AND_CL_VALIDATORS_ATTR,
        CL_BALANCE_AND_CL_VALIDATORS_KEY, CL_BALANCE_AND_CL_VALIDATORS_POSITION, ETH_ADDRESS,
        STAKING_STATE_ATTR, STAKING_STATE_POSITION, STETH_ADDRESS, STETH_COMPONENT_ID,
        TOKEN_TO_TRACK_TOTAL_POOLED_ETH_ATTR, TOTAL_AND_EXTERNAL_SHARES_ATTR,
        TOTAL_AND_EXTERNAL_SHARES_KEY, TOTAL_AND_EXTERNAL_SHARES_POSITION, WSTETH_ADDRESS,
        WSTETH_COMPONENT_ID, WSTETH_SHARES_ATTR, WSTETH_SHARES_KEY, WSTETH_SHARES_POSITION,
    },
    state::{BalanceState, InitialState},
    utils::attribute_with_bytes,
};

/// Creates the stETH and wstETH components on `start_block`, and nothing on any other block.
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let initial_state = InitialState::parse(&params)?;

    if block.number != initial_state.start_block {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] });
    }

    let tx = block
        .transactions()
        .next()
        .ok_or_else(|| anyhow!("Activation block has no transactions"))?;

    Ok(BlockTransactionProtocolComponents {
        tx_components: vec![TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: create_components(),
        }],
    })
}

/// Builds both components, each tagged with the token its `totalPooledEther` is denominated in.
fn create_components() -> Vec<ProtocolComponent> {
    vec![
        ProtocolComponent::new(STETH_COMPONENT_ID)
            .with_tokens(&[STETH_ADDRESS, ETH_ADDRESS])
            .with_attributes(&[(TOKEN_TO_TRACK_TOTAL_POOLED_ETH_ATTR, ETH_ADDRESS.as_ref())])
            .as_swap_type("lido_v3_pool", ImplementationType::Custom),
        ProtocolComponent::new(WSTETH_COMPONENT_ID)
            .with_tokens(&[STETH_ADDRESS, WSTETH_ADDRESS])
            .with_attributes(&[(TOKEN_TO_TRACK_TOTAL_POOLED_ETH_ATTR, STETH_ADDRESS.as_ref())])
            .as_swap_type("lido_v3_pool", ImplementationType::Custom),
    ]
}

/// Carries the latest raw value of every slot that feeds a component balance, so a block that
/// touches only one of them can still report both balances. Seeded from the manifest snapshot on
/// `start_block`.
#[substreams::handlers::store]
pub fn store_balance_slots(params: String, block: eth::v2::Block, store: StoreSetBigInt) {
    let initial_state = InitialState::parse(&params).expect("Failed to parse Lido V3 params");

    if block.number == initial_state.start_block {
        let seed = initial_state
            .balance_state()
            .expect("Failed to decode the Lido V3 initial state");
        store.set(0, TOTAL_AND_EXTERNAL_SHARES_KEY, &seed.total_and_external_shares);
        store.set(
            0,
            BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY,
            &seed.buffered_ether_and_deposited_validators,
        );
        store.set(0, CL_BALANCE_AND_CL_VALIDATORS_KEY, &seed.cl_balance_and_cl_validators);
        store.set(0, WSTETH_SHARES_KEY, &seed.wsteth_shares);
        return;
    }

    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
        {
            for storage_change in call
                .storage_changes
                .iter()
                .filter(|change| change.address == STETH_ADDRESS)
            {
                if let Some(key) = balance_slot_key(&storage_change.key) {
                    store.set(
                        storage_change.ordinal,
                        key,
                        &BigInt::from_unsigned_bytes_be(&storage_change.new_value),
                    );
                }
            }
        }
    }
}

/// The subset of tracked slots that feed the component balances. The stake limit is reported as
/// an attribute but moves no balance, so it maps to `None`.
fn balance_slot_key(slot: &[u8]) -> Option<&'static str> {
    if slot == TOTAL_AND_EXTERNAL_SHARES_POSITION {
        Some(TOTAL_AND_EXTERNAL_SHARES_KEY)
    } else if slot == BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_POSITION {
        Some(BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY)
    } else if slot == CL_BALANCE_AND_CL_VALIDATORS_POSITION {
        Some(CL_BALANCE_AND_CL_VALIDATORS_KEY)
    } else if slot == WSTETH_SHARES_POSITION {
        Some(WSTETH_SHARES_KEY)
    } else {
        None
    }
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

    if !protocol_components
        .tx_components
        .is_empty()
    {
        initialize_protocol_components(
            &initial_state,
            protocol_components,
            &mut transaction_changes,
        )?;
    } else {
        handle_state_updates(&block, &balance_deltas, &balance_store, &mut transaction_changes);
    }

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

/// Registers both components on the activation transaction and seeds them from the manifest
/// snapshot.
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
        attributes: initial_state.steth_creation_attributes()?,
    });
    builder.add_entity_change(&EntityChanges {
        component_id: WSTETH_COMPONENT_ID.to_string(),
        attributes: initial_state.wsteth_creation_attributes()?,
    });

    add_balance_changes(builder, &initial_state.balance_state()?);

    Ok(())
}

/// Turns stETH storage writes into per-transaction attribute and balance changes, routing each
/// attribute to only the components its slot describes.
fn handle_state_updates(
    block: &eth::v2::Block,
    balance_deltas: &StoreDeltas,
    balance_store: &StoreGetBigInt,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let mut balances = block_start_balance_state(balance_deltas, balance_store);

    for tx in block.transactions() {
        let mut balance_slot_touched = false;

        for call in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
        {
            for storage_change in call
                .storage_changes
                .iter()
                .filter(|change| change.address == STETH_ADDRESS)
            {
                let Some((attr_name, target)) = tracked_attribute(&storage_change.key) else {
                    continue;
                };

                let builder = transaction_changes
                    .entry(tx.index as u64)
                    .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

                if target != AttributeTarget::WstEthOnly {
                    builder.add_entity_change(&EntityChanges {
                        component_id: STETH_COMPONENT_ID.to_string(),
                        attributes: vec![attribute_with_bytes(
                            attr_name,
                            &storage_change.new_value,
                            ChangeType::Update,
                        )],
                    });
                }

                if target != AttributeTarget::StEthOnly {
                    builder.add_entity_change(&EntityChanges {
                        component_id: WSTETH_COMPONENT_ID.to_string(),
                        attributes: vec![attribute_with_bytes(
                            attr_name,
                            &storage_change.new_value,
                            ChangeType::Update,
                        )],
                    });
                }

                if let Some(key) = balance_slot_key(&storage_change.key) {
                    let value = BigInt::from_unsigned_bytes_be(&storage_change.new_value);
                    balances.apply(key, value);
                    balance_slot_touched = true;
                }
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

/// Reports both components' absolute balances.
///
/// The stETH component is backed by the whole staking pool, so it reports `totalPooledEther` in
/// ETH. The wstETH component can only ever return the stETH locked in the wrapper, so it reports
/// that, not the pool total - reporting the pool total for both would also double-count the
/// protocol's TVL.
fn add_balance_changes(builder: &mut TransactionChangesBuilder, balances: &BalanceState) {
    builder.add_balance_change(&BalanceChange {
        token: ETH_ADDRESS.to_vec(),
        balance: balances
            .total_pooled_ether()
            .to_signed_bytes_be(),
        component_id: STETH_COMPONENT_ID.as_bytes().to_vec(),
    });
    builder.add_balance_change(&BalanceChange {
        token: STETH_ADDRESS.to_vec(),
        balance: balances
            .wsteth_steth_balance()
            .to_signed_bytes_be(),
        component_id: WSTETH_COMPONENT_ID.as_bytes().to_vec(),
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
            Some(first_delta) => decode_store_value(&first_delta.old_value),
            None => balance_store
                .get_last(key)
                .unwrap_or_else(BigInt::zero),
        }
    };

    BalanceState {
        total_and_external_shares: value_for(TOTAL_AND_EXTERNAL_SHARES_KEY),
        buffered_ether_and_deposited_validators: value_for(
            BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY,
        ),
        cl_balance_and_cl_validators: value_for(CL_BALANCE_AND_CL_VALIDATORS_KEY),
        wsteth_shares: value_for(WSTETH_SHARES_KEY),
    }
}

/// `StoreSetBigInt` serialises values as decimal strings.
fn decode_store_value(bytes: &[u8]) -> BigInt {
    if bytes.is_empty() {
        return BigInt::zero();
    }
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<BigInt>().ok())
        .unwrap_or_else(BigInt::zero)
}

/// Which components an attribute belongs to. The share rate and the pooled-ether accounting drive
/// both components; the stake limit only gates staking, and the wrapper's share balance only
/// describes wstETH.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttributeTarget {
    StEthOnly,
    WstEthOnly,
    Both,
}

/// The attribute a tracked stETH slot maps to, and the components it describes.
fn tracked_attribute(slot: &[u8]) -> Option<(&'static str, AttributeTarget)> {
    if slot == TOTAL_AND_EXTERNAL_SHARES_POSITION {
        Some((TOTAL_AND_EXTERNAL_SHARES_ATTR, AttributeTarget::Both))
    } else if slot == BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_POSITION {
        Some((BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR, AttributeTarget::Both))
    } else if slot == CL_BALANCE_AND_CL_VALIDATORS_POSITION {
        Some((CL_BALANCE_AND_CL_VALIDATORS_ATTR, AttributeTarget::Both))
    } else if slot == STAKING_STATE_POSITION {
        Some((STAKING_STATE_ATTR, AttributeTarget::StEthOnly))
    } else if slot == WSTETH_SHARES_POSITION {
        Some((WSTETH_SHARES_ATTR, AttributeTarget::WstEthOnly))
    } else {
        None
    }
}
