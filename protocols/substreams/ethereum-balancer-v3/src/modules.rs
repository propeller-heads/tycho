use crate::{
    abi::vault_contract::{
        events::{LiquidityAdded, LiquidityRemoved, PoolPausedStateChanged, Swap},
        functions::{Erc4626BufferWrapOrUnwrap, SendTo, Settle},
    },
    pool_factories,
};
use anyhow::Result;
use itertools::Itertools;
use keccak_hash::keccak;
use std::collections::HashMap;
use substreams::{
    hex, log,
    pb::substreams::StoreDeltas,
    store::{
        StoreAddBigInt, StoreGet, StoreGetInt64, StoreGetProto, StoreNew, StoreSetIfNotExists,
        StoreSetIfNotExistsInt64, StoreSetIfNotExistsProto,
    },
};
use substreams_ethereum::{
    pb::eth::{self, v2::StorageChange},
    Event, Function,
};
use tycho_substreams::{
    attributes::json_deserialize_address_list, balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes, contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint, models::entry_point_params::TraceData, prelude::*,
};

pub const VAULT_ADDRESS: &[u8] = &hex!("bA1333333333a1BA1108E8412f11850A5C319bA9");
pub const VAULT_EXTENSION_ADDRESS: &[u8; 20] = &hex!("0E8B07657D719B86e06bF0806D6729e3D528C9A9");
pub const BATCH_ROUTER_ADDRESS: &[u8; 20] = &hex!("136f1efcc3f8f88516b9e94110d56fdbfb1778d1");
pub const PERMIT_2_ADDRESS: &[u8; 20] = &hex!("000000000022D473030F116dDEE9F6B43aC78BA3");

#[substreams::handlers::map]
pub fn map_components(block: eth::v2::Block) -> Result<BlockTransactionProtocolComponents> {
    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        let mut components = Vec::new();
        for (log, call) in tx.logs_with_calls() {
            if let Some(component) =
                pool_factories::address_map(log.address.as_slice(), log, call.call)
            {
                components.push(component);
            }
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// Simply stores the `ProtocolComponent`s with the pool address as the key and the pool id as value
#[substreams::handlers::store]
pub fn store_components(
    map: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    map.tx_components
        .into_iter()
        .for_each(|tx_pc| {
            tx_pc
                .components
                .into_iter()
                .for_each(|pc| store.set_if_not_exists(0, format!("pool:{0}", &pc.id), &pc))
        });
}

/// Set of token that are used by BalancerV3. This is used to filter out account balances updates
/// for unknown tokens.
#[substreams::handlers::store]
pub fn store_token_set(map: BlockTransactionProtocolComponents, store: StoreSetIfNotExistsInt64) {
    map.tx_components
        .into_iter()
        .for_each(|tx_pc| {
            tx_pc
                .components
                .into_iter()
                .for_each(|pc| {
                    pc.tokens
                        .into_iter()
                        .for_each(|token| store.set_if_not_exists(0, hex::encode(token), &1))
                })
        });
}

#[substreams::handlers::map]
pub fn map_relative_balances(
    block: eth::v2::Block,
    store: StoreGetProto<ProtocolComponent>,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let balance_deltas = block
        .logs()
        .filter(|log| log.address() == VAULT_ADDRESS)
        .flat_map(|vault_log| {
            let mut deltas = Vec::new();

            if let Some(Swap { pool, token_in, token_out, amount_in, amount_out, .. }) =
                Swap::match_and_decode(vault_log.log)
            {
                let component_id = format!("0x{}", hex::encode(pool));
                log::info!(
                    "swap at component id: {:?} with key: {:?}",
                    component_id,
                    format!("pool:{}", &component_id)
                );

                if store
                    .get_last(format!("pool:{}", &component_id))
                    .is_some()
                {
                    deltas.extend_from_slice(&[
                        BalanceDelta {
                            ord: vault_log.ordinal(),
                            tx: Some(vault_log.receipt.transaction.into()),
                            token: token_in.to_vec(),
                            delta: amount_in.to_signed_bytes_be(),
                            component_id: component_id.as_bytes().to_vec(),
                        },
                        BalanceDelta {
                            ord: vault_log.ordinal(),
                            tx: Some(vault_log.receipt.transaction.into()),
                            token: token_out.to_vec(),
                            delta: amount_out.neg().to_signed_bytes_be(),
                            component_id: component_id.as_bytes().to_vec(),
                        },
                    ]);
                }
            }
            if let Some(LiquidityAdded { pool, amounts_added_raw, .. }) =
                LiquidityAdded::match_and_decode(vault_log.log)
            {
                let component_id = format!("0x{}", hex::encode(pool));
                if let Some(component) = store.get_last(format!("pool:{}", &component_id)) {
                    if component.tokens.len() != amounts_added_raw.len() {
                        panic!(
                            "liquidity added to pool with different number of tokens than expected"
                        );
                    }
                    log::info!(
                        "liquidity added at component id: {:?} with key: {:?} with tokens: {:?}",
                        component_id,
                        format!("pool:{}", &component_id),
                        component.tokens
                    );
                    let deltas_from_added_liquidity = amounts_added_raw
                        .into_iter()
                        .zip(component.tokens.iter())
                        .map(|(amount, token)| BalanceDelta {
                            ord: vault_log.ordinal(),
                            tx: Some(vault_log.receipt.transaction.into()),
                            token: token.to_vec(),
                            delta: amount.to_signed_bytes_be(),
                            component_id: component_id.as_bytes().to_vec(),
                        })
                        .collect::<Vec<_>>();
                    deltas.extend_from_slice(&deltas_from_added_liquidity);
                }
            }
            if let Some(LiquidityRemoved { pool, amounts_removed_raw, .. }) =
                LiquidityRemoved::match_and_decode(vault_log.log)
            {
                let component_id = format!("0x{}", hex::encode(pool));
                log::info!(
                    "liquidity removed at component id: {:?} with key: {:?}",
                    component_id,
                    format!("pool:{}", &component_id)
                );
                if let Some(component) = store.get_last(format!("pool:{}", &component_id)) {
                    if component.tokens.len() != amounts_removed_raw.len() {
                        panic!(
                            "liquidity removed from pool with different number of tokens than expected"
                        );
                    }
                    let deltas_from_removed_liquidity = amounts_removed_raw
                        .into_iter()
                        .zip(component.tokens.iter())
                        .map(|(amount, token)| BalanceDelta {
                            ord: vault_log.ordinal(),
                            tx: Some(vault_log.receipt.transaction.into()),
                            token: token.to_vec(),
                            delta: amount.neg().to_signed_bytes_be(),
                            component_id: component_id.as_bytes().to_vec(),
                        })
                        .collect::<Vec<_>>();
                    deltas.extend_from_slice(&deltas_from_removed_liquidity);
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
///  cooresponding changes (balance, component, contract), inserting a new one if it doesn't exist.
///  At the very end, the map can easily be sorted by index to ensure the final
/// `BlockChanges`  is ordered by transactions properly.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::v2::Block,
    grouped_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetProto<ProtocolComponent>,
    tokens_store: StoreGetInt64,
    balance_store: StoreDeltas, // Note, this map module is using the `deltas` mode for the store.
) -> Result<BlockChanges> {
    // We merge contract changes by transaction (identified by transaction index) making it easy to
    //  sort them at the very end.
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // Handle pool pause state changes
    block
        .logs()
        .filter(|log| log.address() == VAULT_ADDRESS)
        .for_each(|log| {
            if let Some(PoolPausedStateChanged { pool, paused }) =
                PoolPausedStateChanged::match_and_decode(log)
            {
                let component_id = format!("0x{}", hex::encode(&pool));
                let tx: Transaction = log.receipt.transaction.into();
                if components_store
                    .get_last(format!("pool:{}", &component_id))
                    .is_some()
                {
                    let builder = transaction_changes
                        .entry(tx.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(&tx));

                    let entity_change = EntityChanges {
                        component_id,
                        attributes: vec![Attribute {
                            name: "paused".to_string(),
                            value: vec![1u8],
                            change: if paused {
                                ChangeType::Creation.into()
                            } else {
                                ChangeType::Deletion.into()
                            },
                        }],
                    };
                    builder.add_entity_change(&entity_change);
                }
            }
        });

    // `ProtocolComponents` are gathered from `map_pools_created` which just need a bit of work to
    //   convert into `TransactionChanges`
    let default_attributes = vec![
        Attribute {
            // TODO: remove this and track account_balances instead
            name: "balance_owner".to_string(),
            value: VAULT_ADDRESS.to_vec(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "stateless_contract_addr_0".into(),
            value: address_to_bytes_with_0x(VAULT_EXTENSION_ADDRESS),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "stateless_contract_addr_1".into(),
            value: address_to_bytes_with_0x(BATCH_ROUTER_ADDRESS),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "stateless_contract_addr_2".into(),
            value: address_to_bytes_with_0x(PERMIT_2_ADDRESS),
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

    // Balance changes are gathered by the `StoreDelta` based on `PoolBalanceChanged` creating
    //  `BlockBalanceDeltas`. We essentially just process the changes that occurred to the `store`
    // this  block. Then, these balance changes are merged onto the existing map of tx contract
    // changes,  inserting a new one if it doesn't exist.
    aggregate_balances_changes(balance_store, deltas)
        .iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map.values().for_each(|bc| {
                        builder.add_balance_change(bc);
                    })
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

    // Extract token balances for balancer v3 vault
    block
        .transaction_traces
        .iter()
        .for_each(|tx| {
            let vault_balance_change_per_tx =
                get_vault_reserves(tx, &components_store, &tokens_store);

            if !vault_balance_change_per_tx.is_empty() {
                let tycho_tx = Transaction::from(tx);
                let builder = transaction_changes
                    .entry(tx.index.into())
                    .or_insert_with(|| TransactionChangesBuilder::new(&tycho_tx));

                let mut vault_contract_tlv_changes =
                    InterimContractChange::new(VAULT_ADDRESS, false);
                for (token_addr, reserve_value) in vault_balance_change_per_tx {
                    vault_contract_tlv_changes.upsert_token_balance(
                        token_addr.as_slice(),
                        reserve_value.value.as_slice(),
                    );
                }
                builder.add_contract_changes(&vault_contract_tlv_changes);
            }
        });

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
                            .map(|c| c.id)
                            .unwrap(); // Shouldn't happen because we filter by known components
                                       // in `extract_contract_changes_builder`
                        change.mark_component_as_updated(&id);
                    }
                })
        });

    let block_storage_changes = get_block_storage_changes(&block);

    // Process all `transaction_changes` for final output in the `BlockChanges`,
    //  sorted by transaction index (the key).
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

/// Converts address bytes into a Vec<u8> containing a leading `0x`.
fn address_to_bytes_with_0x(address: &[u8; 20]) -> Vec<u8> {
    address_to_string_with_0x(address).into_bytes()
}

/// Converts address bytes into a string containing a leading `0x`.
fn address_to_string_with_0x(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

// function needed to match reservesOf in vault storage, which by definition
// they should always be equal to the `token.balanceOf(this)` except during unlock
fn get_vault_reserves(
    transaction: &eth::v2::TransactionTrace,
    store: &StoreGetProto<ProtocolComponent>,
    token_store: &StoreGetInt64,
) -> HashMap<Vec<u8>, ReserveValue> {
    // reservesOf mapping for the current block Address -> Balance
    let mut reserves_of = HashMap::new();
    transaction
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .filter(|call| call.address == VAULT_ADDRESS)
        .for_each(|call| {
            if let Some(Settle { token, .. }) = Settle::match_and_decode(call) {
                for change in &call.storage_changes {
                    add_change_if_accounted(
                        &mut reserves_of,
                        change,
                        token.as_slice(),
                        token_store,
                    );
                }
            }
            if let Some(SendTo { token, .. }) = SendTo::match_and_decode(call) {
                for change in &call.storage_changes {
                    add_change_if_accounted(
                        &mut reserves_of,
                        change,
                        token.as_slice(),
                        token_store,
                    );
                }
            }
            if let Some(Erc4626BufferWrapOrUnwrap { params }) =
                Erc4626BufferWrapOrUnwrap::match_and_decode(call)
            {
                for change in &call.storage_changes {
                    let wrapped_token = params.2.clone();
                    let component_id = format!("0x{}", hex::encode(&wrapped_token));
                    if let Some(component) = store.get_last(component_id) {
                        let underlying_token = component.tokens[1].clone();
                        add_change_if_accounted(
                            &mut reserves_of,
                            change,
                            wrapped_token.as_slice(),
                            token_store,
                        );
                        add_change_if_accounted(
                            &mut reserves_of,
                            change,
                            underlying_token.as_slice(),
                            token_store,
                        );
                    }
                }
            }
        });
    reserves_of
}

struct ReserveValue {
    ordinal: u64,
    value: Vec<u8>,
}

fn add_change_if_accounted(
    reserves_of: &mut HashMap<Vec<u8>, ReserveValue>,
    change: &StorageChange,
    token_address: &[u8],
    token_store: &StoreGetInt64,
) {
    let slot_key = get_storage_key_for_token(token_address);
    // record changes happening on vault contract at reserves_of storage key
    if change.key == slot_key && token_store.has_last(hex::encode(token_address)) {
        reserves_of
            .entry(token_address.to_vec())
            .and_modify(|v| {
                if v.ordinal < change.ordinal {
                    v.value = change.new_value.clone();
                    v.ordinal = change.ordinal;
                }
            })
            .or_insert(ReserveValue { value: change.new_value.clone(), ordinal: change.ordinal });
    }
}

// token_addr -> keccak256(abi.encode(token_address, 8)) as 8 is the order in which reserves of are
// declared
fn get_storage_key_for_token(token_address: &[u8]) -> Vec<u8> {
    let mut input = [0u8; 64];
    input[12..32].copy_from_slice(token_address);
    input[63] = 8u8;
    let result = keccak(input.as_slice())
        .as_bytes()
        .to_vec();
    result
}
