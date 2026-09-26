//! Substreams modules for Camelot V3 (Algebra V1.9) on Arbitrum One.
//!
//! VM integration: every pool component links the pool, its `DataStorageOperator` and the
//! factory, and the code and storage of all three are emitted so the pool can be simulated in an
//! empty VM. Pool token balances are tracked from ERC20 `Transfer` events on both sides of a
//! transfer, so mint, burn, collect, swap, flash, community fee payments and pool-to-pool
//! transfers are all covered by the same rule. Pools use `manual_updates`: a pool is marked
//! for refresh when its own or its operator's storage changed, not on factory changes.
use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use itertools::Itertools;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{
        StoreAddBigInt, StoreGet, StoreGetProto, StoreNew, StoreSetIfNotExists,
        StoreSetIfNotExistsProto,
    },
};
use substreams_ethereum::{pb::eth::v2::Block, Event};
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    abi::erc20, balances::aggregate_balances_changes, contract::extract_contract_changes_builder,
    prelude::*,
};

use crate::{
    camelot::{
        active_incentive_from_slot, contract_key, pools_created, ACTIVE_INCENTIVE_ATTRIBUTE,
        ACTIVE_INCENTIVE_SLOT, DATA_STORAGE_OPERATOR_ATTRIBUTE,
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

/// Merges new components, absolute balances, contract changes, incentive updates and update
/// markers into one `TransactionChanges` per transaction, sorted by transaction index.
#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: Block,
    new_components: BlockTransactionProtocolComponents,
    balance_deltas: BlockBalanceDeltas,
    components: StoreGetProto<ProtocolComponent>,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let factory = parse_factory(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

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

    extract_contract_changes_builder(
        &block,
        |address| {
            address == factory ||
                components
                    .get_last(contract_key(&address.to_hex()))
                    .is_some()
        },
        &mut transaction_changes,
    );

    // A pool can only write its incentive slot in a transaction whose contract changes were
    // extracted above, so every other transaction is skipped without scanning its storage.
    for tx in block.transactions() {
        if !transaction_changes.contains_key(&u64::from(tx.index)) {
            continue;
        }
        let created: HashSet<&str> = new_components
            .tx_components
            .iter()
            .filter(|tx_components| {
                tx_components
                    .tx
                    .as_ref()
                    .map(|t| t.index) ==
                    Some(u64::from(tx.index))
            })
            .flat_map(|tx_components| {
                tx_components
                    .components
                    .iter()
                    .map(|c| c.id.as_str())
            })
            .collect();
        let mut incentives: HashMap<String, (u64, Vec<u8>)> = HashMap::new();
        let slot_writes = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.key == ACTIVE_INCENTIVE_SLOT);
        for change in slot_writes {
            let Some(component) = pool_at(&components, &change.address) else {
                continue;
            };
            let latest = incentives
                .entry(component.id)
                .or_insert((0, Vec::new()));
            if change.ordinal >= latest.0 {
                *latest = (change.ordinal, change.new_value.clone());
            }
        }
        if incentives.is_empty() {
            continue;
        }
        let builder = transaction_changes
            .entry(tx.index.into())
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));
        for (component_id, (_, value)) in incentives {
            // The pool writes the slot in its creation transaction, so a new pool always
            // starts with a known value.
            let change = if created.contains(component_id.as_str()) {
                ChangeType::Creation
            } else {
                ChangeType::Update
            };
            builder.add_entity_change(&EntityChanges {
                component_id,
                attributes: vec![Attribute {
                    name: ACTIVE_INCENTIVE_ATTRIBUTE.to_string(),
                    value: active_incentive_from_slot(&value)?,
                    change: change.into(),
                }],
            });
        }
    }

    // Factory storage is not a reason to refresh a pool: its vault address only decides where
    // community fees go, and its default community fee, base fee configuration and farming
    // address only affect pools created or configured later.
    for builder in transaction_changes.values_mut() {
        let changed: Vec<Vec<u8>> = builder
            .changed_contracts()
            .map(|address| address.to_vec())
            .collect();
        for address in changed {
            if address == factory {
                continue;
            }
            let component = components
                .get_last(contract_key(&address.to_hex()))
                .ok_or_else(|| {
                    anyhow!(
                        "contract 0x{} changed but belongs to no component",
                        hex::encode(&address)
                    )
                })?;
            builder.mark_component_as_updated(&component.id);
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .into_iter()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        // Raw per-block storage changes only feed the Dynamic Contract Indexer, which this
        // package does not use: every contract a swap touches is indexed directly.
        storage_changes: vec![],
    })
}
