use crate::{abi, pool_factories};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    hex,
    pb::substreams::StoreDeltas,
    store::{StoreAddBigInt, StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString},
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    attributes::json_deserialize_address_list, balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes, contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint, models::entry_point_params::TraceData, prelude::*,
};

pub const VAULT_ADDRESS: &[u8] = &hex!("BA12222222228d8Ba445958a75a0704d566BF2C8");
pub const ZERO_ADDRESS: &[u8] = &[0u8; 20];

#[substreams::handlers::map]
pub fn map_components(block: eth::v2::Block) -> Result<BlockTransactionProtocolComponents> {
    // Gather contract changes by indexing `PoolCreated` events and analysing the `Create` call
    // We store these as a hashmap by tx hash since we need to agg by tx hash later
    Ok(BlockTransactionProtocolComponents {
        tx_components: block
            .transactions()
            .filter_map(|tx| {
                let components = tx
                    .logs_with_calls()
                    .filter_map(|(log, call)| {
                        pool_factories::address_map(
                            call.call.address.as_slice(),
                            log,
                            call.call,
                            tx,
                        )
                    })
                    .collect::<Vec<_>>();

                if !components.is_empty() {
                    Some(TransactionProtocolComponents { tx: Some(tx.into()), components })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
    })
}

/// Simply stores the `ProtocolComponent`s with the pool address as the key and the pool id as value
#[substreams::handlers::store]
pub fn store_components(map: BlockTransactionProtocolComponents, store: StoreSetString) {
    map.tx_components
        .into_iter()
        .for_each(|tx_pc| {
            tx_pc
                .components
                .into_iter()
                .for_each(|pc| store.set(0, format!("pool:{0}", &pc.id[..42]), &pc.id))
        });
}

/// Since the `PoolBalanceChanged` and `Swap` events administer only deltas, we need to leverage a
/// map and a store to be able to tally up final balances for tokens in a pool.
#[substreams::handlers::map]
pub fn map_relative_balances(
    block: eth::v2::Block,
    store: StoreGetString,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let balance_deltas = block
        .logs()
        .filter(|log| log.address() == VAULT_ADDRESS)
        .flat_map(|vault_log| {
            let mut deltas = Vec::new();

            if let Some(ev) =
                abi::vault::events::PoolBalanceChanged::match_and_decode(vault_log.log)
            {
                let component_id = format!("0x{}", hex::encode(ev.pool_id));
                let bpt_token = hex::decode(&component_id[2..42]).unwrap();

                if store
                    .get_last(format!("pool:{}", &component_id[..42]))
                    .is_some()
                {
                    for (token, delta) in ev.tokens.iter().zip(ev.deltas.iter()) {
                        // BPT tokens not supported - their balance handling is currently bugged
                        if *token != bpt_token {
                            deltas.push(BalanceDelta {
                                ord: vault_log.ordinal(),
                                tx: Some(vault_log.receipt.transaction.into()),
                                token: token.to_vec(),
                                delta: delta.to_signed_bytes_be(),
                                component_id: component_id.as_bytes().to_vec(),
                            });
                        }
                    }
                }
            } else if let Some(ev) = abi::vault::events::Swap::match_and_decode(vault_log.log) {
                let component_id = format!("0x{}", hex::encode(ev.pool_id));
                let bpt_token = hex::decode(&component_id[2..42]).unwrap();

                if store
                    .get_last(format!("pool:{}", &component_id[..42]))
                    .is_some()
                {
                    // BPT tokens not supported - their balance handling is currently bugged
                    if ev.token_in != bpt_token {
                        deltas.push(BalanceDelta {
                            ord: vault_log.ordinal(),
                            tx: Some(vault_log.receipt.transaction.into()),
                            token: ev.token_in.to_vec(),
                            delta: ev.amount_in.to_signed_bytes_be(),
                            component_id: component_id.as_bytes().to_vec(),
                        });
                    }
                    // BPT tokens not supported - their balance handling is currently bugged
                    if ev.token_out != bpt_token {
                        deltas.push(BalanceDelta {
                            ord: vault_log.ordinal(),
                            tx: Some(vault_log.receipt.transaction.into()),
                            token: ev.token_out.to_vec(),
                            delta: ev.amount_out.neg().to_signed_bytes_be(),
                            component_id: component_id.as_bytes().to_vec(),
                        });
                    }
                }
            } else if let Some(ev) =
                abi::vault::events::PoolBalanceManaged::match_and_decode(vault_log.log)
            {
                let component_id = format!("0x{}", hex::encode(ev.pool_id));
                let bpt_token = hex::decode(&component_id[2..42]).unwrap();
                if store
                    .get_last(format!("pool:{}", &component_id[..42]))
                    .is_some()
                    // BPT tokens not supported - their balance handling is currently bugged
                    && ev.token != bpt_token
                {
                    deltas.push(BalanceDelta {
                        ord: vault_log.ordinal(),
                        tx: Some(vault_log.receipt.transaction.into()),
                        token: ev.token.to_vec(),
                        delta: ev.cash_delta.to_signed_bytes_be(),
                        component_id: component_id.as_bytes().to_vec(),
                    });
                }
            }

            deltas
        })
        .collect::<Vec<_>>();

    Ok(BlockBalanceDeltas { balance_deltas })
}

/// It's significant to include both the `pool_id` and the `token_id` for each balance delta as the
///  store key to ensure that there's a unique balance being tallied for each.
#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

/// This is the main map that handles most of the indexing of this substream.
/// Every contract change is grouped by transaction index via the `transaction_changes`
///  map. Each block of code will extend the `TransactionChanges` struct with the
///  corresponding changes (balance, component, contract), inserting a new one if it doesn't exist.
///  At the very end, the map can easily be sorted by index to ensure the final
/// `BlockChanges`  is ordered by transactions properly.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::v2::Block,
    grouped_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetString,
    balance_store: StoreDeltas, // Note, this map module is using the `deltas` mode for the store.
) -> Result<BlockChanges> {
    // We merge contract changes by transaction (identified by transaction index) making it easy to
    //  sort them at the very end.
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // `ProtocolComponents` are gathered from `map_pools_created` which just need a bit of work to
    //   convert into `TransactionChanges`
    let default_attributes = vec![
        Attribute {
            name: "balance_owner".to_string(),
            value: VAULT_ADDRESS.to_vec(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "update_marker".to_string(),
            value: vec![1u8],
            change: ChangeType::Creation.into(),
        },
    ];
    grouped_components
        .tx_components
        .iter()
        .for_each(|tx_component| {
            // initialise builder if not yet present for this tx
            let tx = tx_component.tx.as_ref().unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            // iterate over individual components created within this tx
            tx_component
                .components
                .iter()
                .for_each(|component| {
                    let rate_providers = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "rate_providers")
                        .map(|att| json_deserialize_address_list(&att.value));

                    if let Some(rate_providers) = rate_providers {
                        for rate_provider in rate_providers {
                            if rate_provider == ZERO_ADDRESS {
                                // Skipped: is not a rate provider
                                continue;
                            }

                            let trace_data = TraceData::Rpc(RpcTraceData {
                                caller: None,
                                calldata: hex::decode("679aefce").unwrap(), // getRate()
                            });
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                rate_provider,
                                "getRate()".to_string(),
                                component.id.clone(),
                                trace_data,
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
                        }
                    }
                    builder.add_protocol_component(component);
                    let entity_change = EntityChanges {
                        component_id: component.id.clone(),
                        attributes: default_attributes.clone(),
                    };
                    builder.add_entity_change(&entity_change)
                });
        });

    // `SwapFeePercentageChanged` is emitted during the pool creation tx, so we don't need to
    // set a static `fee` attribute in the pool factory. Instead, we rely on this event being
    // picked up here in `map_protocol_changes` to set the fee as a dynamic attribute.
    block.transactions().for_each(|tx| {
        let mut logs_matched = Vec::new();
        for (log, _) in tx.logs_with_calls() {
            if let Some(ev) =
                abi::base_pool::events::SwapFeePercentageChanged::match_and_decode(log)
            {
                let pool_addr_key = format!("pool:0x{}", hex::encode(&log.address));
                if let Some(pool_id) = components_store.get_last(&pool_addr_key) {
                    logs_matched.push((pool_id, ev));
                }
            }
        }

        if !logs_matched.is_empty() {
            let tycho_tx: tycho_substreams::models::Transaction = tx.into();
            let builder = transaction_changes
                .entry(tycho_tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tycho_tx));

            for (pool_id, ev) in logs_matched {
                let entity_change = EntityChanges {
                    component_id: pool_id.clone(),
                    attributes: vec![Attribute {
                        name: "fee".to_string(),
                        value: ev
                            .swap_fee_percentage
                            .to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    }],
                };
                builder.add_entity_change(&entity_change);
            }
        }
    });

    // Balance changes are gathered by the `StoreDelta` based on `PoolBalanceChanged` creating
    // `BlockBalanceDeltas`. We essentially just process the changes that occurred to the `store`
    // this  block. Then, these balance changes are merged onto the existing map of tx contract
    // changes,  inserting a new one if it doesn't exist.
    aggregate_balances_changes(balance_store, deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    // Extract and insert any storage changes that happened for any of the components.
    extract_contract_changes_builder(
        &block,
        |addr| {
            components_store
                .get_last(format!("pool:0x{0}", hex::encode(addr)))
                .is_some() ||
                addr.eq(VAULT_ADDRESS)
        },
        &mut transaction_changes,
    );

    transaction_changes
        .iter_mut()
        .for_each(|(_, change)| {
            // this indirection is necessary due to borrowing rules.
            let addresses = change
                .changed_contracts()
                .map(|e| e.to_vec())
                .collect::<Vec<_>>();
            addresses
                .into_iter()
                .for_each(|address| {
                    if address != VAULT_ADDRESS {
                        // We reconstruct the component_id from the address here
                        let id = components_store
                            .get_last(format!("pool:0x{}", hex::encode(address)))
                            .unwrap(); // Shouldn't happen because we filter by known components in
                                       // `extract_contract_changes_builder`
                        change.mark_component_as_updated(&id);
                    }
                })
        });

    // Process all `transaction_changes` for final output in the `BlockChanges`,
    //  sorted by transaction index (the key).
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
