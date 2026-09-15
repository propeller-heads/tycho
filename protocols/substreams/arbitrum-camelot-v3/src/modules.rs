//! Substreams modules for Camelot V3 (Algebra V1.9) on Arbitrum One.
//!
//! VM integration: every pool component links the pool, its `DataStorageOperator` and the
//! factory, and the code and storage of all three are emitted so the pool can be simulated in an
//! empty VM. Pool token balances are tracked from ERC20 `Transfer` events on both sides of a
//! transfer, so mint, burn, collect, swap, flash, community fee payments and pool-to-pool
//! transfers are all covered by the same rule.
use std::collections::HashMap;

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
    abi::pool::events::Incentive,
    camelot::{
        operator_key, pool_key, pools_created, ACTIVE_INCENTIVE_ATTRIBUTE,
        DATA_STORAGE_OPERATOR_ATTRIBUTE,
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
            store.set_if_not_exists(0, pool_key(&component.id), &component);
            if let Some(operator) = component.get_attribute_value(DATA_STORAGE_OPERATOR_ATTRIBUTE) {
                store.set_if_not_exists(0, operator_key(&operator), &component);
            }
        }
    }
}

/// Emits a relative balance delta for every ERC20 transfer of a pool token into or out of a
/// tracked pool. Both sides of a transfer are handled independently, so a transfer between two
/// pools debits one and credits the other. A self-transfer changes nothing and emits nothing.
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
        let tx: Transaction = log.receipt.transaction.into();
        let movements = [
            (&transfer.from, transfer.value.clone().neg()),
            (&transfer.to, transfer.value.clone()),
        ];
        for (account, delta) in movements {
            let Some(component) = components.get_last(pool_key(&account.to_hex())) else {
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
                tx: Some(tx.clone()),
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

/// Merges new components, absolute balances, contract changes and incentive updates into one
/// `TransactionChanges` per transaction, sorted by transaction index.
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
                    .get_last(pool_key(&address.to_hex()))
                    .is_some() ||
                components
                    .get_last(operator_key(address))
                    .is_some()
        },
        &mut transaction_changes,
    );

    for log in block.logs() {
        let Some(incentive) = Incentive::match_and_decode(log.log) else {
            continue;
        };
        let Some(component) = components.get_last(pool_key(&log.address().to_hex())) else {
            continue;
        };
        let tx: Transaction = log.receipt.transaction.into();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        builder.add_entity_change(&EntityChanges {
            component_id: component.id,
            attributes: vec![Attribute {
                name: ACTIVE_INCENTIVE_ATTRIBUTE.to_string(),
                value: incentive.virtual_pool_address,
                change: ChangeType::Update.into(),
            }],
        });
    }

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
                .get_last(pool_key(&address.to_hex()))
                .or_else(|| components.get_last(operator_key(&address)))
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
