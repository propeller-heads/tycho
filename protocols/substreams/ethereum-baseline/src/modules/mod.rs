//! Baseline/Mercury Substreams handlers.
//!
//! Baseline uses a singleton relay/proxy. Components are bTokens, while contract
//! code, balances, and quote storage live behind the relay.

mod balances;
mod dci;
mod quote_state;

use crate::{pool_factories, pool_factories::DeploymentConfig};
use anyhow::Result;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    prelude::*,
    store::{StoreGetString, StoreSetString},
};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes,
    contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

#[derive(Clone, Copy)]
enum QuoteStateAttributeChange {
    Creation,
    Update,
}

impl QuoteStateAttributeChange {
    fn change_type(self) -> ChangeType {
        match self {
            Self::Creation => ChangeType::Creation,
            Self::Update => ChangeType::Update,
        }
    }
}

/// Find and create all relevant protocol components.
#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config = serde_qs::from_str(params.as_str())?;
    let mut tx_components_by_index: HashMap<u64, TransactionProtocolComponents> = HashMap::new();

    block.logs().for_each(|log| {
        let Some(component) = pool_factories::maybe_create_component(log.log, &config) else {
            return;
        };

        let tx: Transaction = log.receipt.transaction.into();
        tx_components_by_index
            .entry(tx.index)
            .or_insert_with(|| TransactionProtocolComponents {
                tx: Some(tx),
                components: Vec::new(),
            })
            .components
            .push(component);
    });

    Ok(BlockTransactionProtocolComponents {
        tx_components: tx_components_by_index
            .into_iter()
            .sorted_unstable_by_key(|(index, _)| *index)
            .map(|(_, tx_components)| tx_components)
            .collect(),
    })
}

#[substreams::handlers::store]
fn store_pool_reserves(
    map_protocol_components: BlockTransactionProtocolComponents,
    store: StoreSetString,
) {
    map_protocol_components
        .tx_components
        .into_iter()
        .flat_map(|tx_pc| tx_pc.components)
        .for_each(|component| {
            if let Some(reserve) = component.tokens.get(1) {
                store.set(0, format!("reserve:{}", component.id), &hex::encode(reserve));
            }
        });
}

/// Extracts balance changes per component.
#[substreams::handlers::map]
fn map_relative_component_balance(
    params: String,
    block: eth::v2::Block,
    reserve_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
    let res = block
        .transactions()
        .flat_map(|tx| {
            tx.logs_with_calls()
                .flat_map(|(log, _call)| {
                    balances::extract_balance_deltas(&config, tx, log, &reserve_store)
                })
        })
        .collect::<Vec<_>>();

    Ok(BlockBalanceDeltas { balance_deltas: res })
}

/// Aggregates relative balance values into absolute values.
#[substreams::handlers::store]
pub fn store_component_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

/// Aggregates protocol components and balance changes by transaction.
#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    let mut latest_quote_state_tx: HashMap<String, (u64, QuoteStateAttributeChange)> =
        HashMap::new();

    new_components
        .tx_components
        .iter()
        .for_each(|tx_component| {
            let tx = tx_component.tx.as_ref().unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            tx_component
                .components
                .iter()
                .for_each(|component| {
                    builder.add_protocol_component(component);
                    builder.add_entity_change(&EntityChanges {
                        component_id: component.id.clone(),
                        attributes: vec![Attribute {
                            name: "balance_owner".to_string(),
                            value: config.relay_address.clone(),
                            change: ChangeType::Creation.into(),
                        }],
                    });
                    latest_quote_state_tx.insert(
                        component.id.clone(),
                        (tx.index, QuoteStateAttributeChange::Creation),
                    );

                    let Some(b_token) = component.tokens.first() else {
                        return;
                    };

                    dci::QUOTE_ENTRYPOINTS
                        .iter()
                        .for_each(|(signature, selector)| {
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                config.relay_address.clone(),
                                signature.to_string(),
                                component.id.clone(),
                                TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: dci::quote_calldata(*selector, b_token),
                                }),
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
                        });

                    dci::LENS_ENTRYPOINTS
                        .iter()
                        .for_each(|(signature, selector)| {
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                config.relay_address.clone(),
                                signature.to_string(),
                                component.id.clone(),
                                TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: dci::btoken_calldata(*selector, b_token),
                                }),
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
                        });

                    dci::STAKING_ENTRYPOINTS
                        .iter()
                        .for_each(|(signature, selector)| {
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                config.relay_address.clone(),
                                signature.to_string(),
                                component.id.clone(),
                                TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: dci::btoken_calldata(*selector, b_token),
                                }),
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
                        });
                });
        });

    aggregate_balances_changes(balance_store, deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            let mut contract_changes = InterimContractChange::new(&config.relay_address, false);
            let mut updated_component_ids = HashSet::new();
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map.values().for_each(|bc| {
                        builder.add_balance_change(bc);
                        let component_id =
                            String::from_utf8(bc.component_id.clone()).expect("bad component id");
                        builder.mark_component_as_updated(&component_id);
                        updated_component_ids.insert(component_id);
                        contract_changes
                            .upsert_token_balance(bc.token.as_slice(), bc.balance.as_slice())
                    })
                });
            updated_component_ids
                .into_iter()
                .for_each(|component_id| {
                    latest_quote_state_tx
                        .entry(component_id)
                        .and_modify(|(index, _change)| *index = (*index).max(tx.index))
                        .or_insert((tx.index, QuoteStateAttributeChange::Update));
                });
            builder.add_contract_changes(&contract_changes);
        });

    block.transactions().for_each(|tx| {
        tx.logs_with_calls()
            .filter_map(|(log, _call)| {
                if log.address != config.relay_address {
                    return None;
                }
                quote_state::maybe_update_component_id(log)
            })
            .for_each(|component_id| {
                let tx: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(tx.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&tx));
                builder.mark_component_as_updated(&component_id);
                latest_quote_state_tx
                    .entry(component_id)
                    .and_modify(|(index, _change)| *index = (*index).max(tx.index))
                    .or_insert((tx.index, QuoteStateAttributeChange::Update));
            });
    });

    latest_quote_state_tx
        .into_iter()
        .for_each(|(component_id, (tx_index, change))| {
            let Some(builder) = transaction_changes.get_mut(&tx_index) else {
                return;
            };
            let quote_state_attributes = quote_state::attributes_for_component(
                &config.relay_address,
                &component_id,
                change.change_type(),
            );
            if !quote_state_attributes.is_empty() {
                builder.add_entity_change(&EntityChanges {
                    component_id,
                    attributes: quote_state_attributes,
                });
            }
        });

    extract_contract_changes_builder(
        &block,
        |addr| addr == config.relay_address,
        &mut transaction_changes,
    );

    let block_storage_changes = get_block_storage_changes(&block);

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: block_storage_changes,
    })
}
