use crate::{
    abi::vault_contract::{
        events::{LiquidityAddedToBuffer, PoolPausedStateChanged},
        functions::{Erc4626BufferWrapOrUnwrap, SendTo, Settle},
    },
    constants::{BATCH_ROUTER_ADDRESS, PERMIT_2_ADDRESS, VAULT_ADDRESS, VAULT_EXTENSION_ADDRESS},
    pool_balances, pool_factories,
    utils::{
        address_id, buffer_mapping_key, decode_address_from_storage_word,
        mapping_storage_key_for_address, pool_store_key,
    },
};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{
        StoreAddBigInt, StoreGet, StoreGetInt64, StoreGetProto, StoreGetString, StoreNew, StoreSet,
        StoreSetIfNotExists, StoreSetIfNotExistsInt64, StoreSetIfNotExistsProto, StoreSetString,
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
                .for_each(|pc| store.set_if_not_exists(0, format!("pool:{}", pc.id), &pc))
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

#[substreams::handlers::store]
pub fn store_token_mapping(block: eth::v2::Block, store: StoreSetString) {
    block.transactions().for_each(|tx| {
        tx.logs_with_calls()
            .filter(|(log, _)| log.address.as_slice() == VAULT_ADDRESS)
            .for_each(|(log, call)| {
                // The first liquidity add initializes an ERC4626 buffer and emits the wrapped
                // token. The underlying token is not part of the event, so we read the
                // `_bufferAssets[wrapped_token]` storage write from the same Vault call and
                // persist that relationship for later reserve updates.
                if let Some(LiquidityAddedToBuffer { wrapped_token, .. }) =
                    LiquidityAddedToBuffer::match_and_decode(log)
                {
                    if let Some(underlying_token) = find_underlying_token(call.call, &wrapped_token)
                    {
                        store.set(
                            0,
                            buffer_mapping_key(&wrapped_token),
                            &hex::encode(underlying_token),
                        );
                    }
                }
            })
    });
}

#[substreams::handlers::map]
pub fn map_pool_balance_seed_events(
    block: eth::v2::Block,
    store: StoreGetProto<ProtocolComponent>,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    Ok(BlockBalanceDeltas {
        balance_deltas: pool_balances::pool_balance_seed_deltas(&block, &store),
    })
}

#[substreams::handlers::store]
pub fn store_seeded_pool_balances(deltas: BlockBalanceDeltas, store: StoreSetIfNotExistsInt64) {
    deltas
        .balance_deltas
        .into_iter()
        .for_each(|delta| {
            let component_id = String::from_utf8(delta.component_id)
                .expect("delta.component_id is not valid utf-8!");
            store.set_if_not_exists(delta.ord, component_id, &1);
        });
}

#[substreams::handlers::map]
pub fn map_relative_balances(
    block: eth::v2::Block,
    components_store: StoreGetProto<ProtocolComponent>,
    seeded_pool_balances: StoreGetInt64,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    Ok(BlockBalanceDeltas {
        balance_deltas: pool_balances::relative_pool_balance_deltas(
            &block,
            &components_store,
            &seeded_pool_balances,
        ),
    })
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
    token_mapping_store: StoreGetString,
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
                let component_id = address_id(&pool);
                let tx: Transaction = log.receipt.transaction.into();
                if components_store
                    .get_last(format!("pool:{component_id}"))
                    .is_some()
                {
                    let builder = transaction_changes
                        .entry(tx.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(&tx));

                    builder.change_component_pause_state(&component_id, paused);
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
            value: address_id(VAULT_EXTENSION_ADDRESS).into_bytes(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "stateless_contract_addr_1".into(),
            value: address_id(BATCH_ROUTER_ADDRESS).into_bytes(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "stateless_contract_addr_2".into(),
            value: address_id(PERMIT_2_ADDRESS).into_bytes(),
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
                .get_last(pool_store_key(addr))
                .is_some()
                || addr.eq(VAULT_ADDRESS)
        },
        &mut transaction_changes,
    );

    // Extract token balances for balancer v3 vault
    block
        .transaction_traces
        .iter()
        .for_each(|tx| {
            let vault_balance_change_per_tx =
                get_vault_reserves(tx, &tokens_store, &token_mapping_store);

            if !vault_balance_change_per_tx.is_empty() {
                let tycho_tx = Transaction::from(tx);
                let builder = transaction_changes
                    .entry(tx.index.into())
                    .or_insert_with(|| TransactionChangesBuilder::new(&tycho_tx));

                let mut vault_contract_tlv_changes =
                    InterimContractChange::new(VAULT_ADDRESS, false);
                for (token_addr, reserve_value) in vault_balance_change_per_tx {
                    vault_contract_tlv_changes
                        .upsert_token_balance(token_addr.as_slice(), reserve_value.as_slice());
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
                            .get_last(pool_store_key(&address))
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

const RESERVES_OF_SLOT: u8 = 8;
const BUFFER_ASSETS_SLOT: u8 = 14;

struct ReserveValue {
    ordinal: u64,
    value: Vec<u8>,
}

// Match reservesOf in Vault storage. By definition this should equal
// `token.balanceOf(address(this))`, except during Vault unlock execution.
fn get_vault_reserves(
    transaction: &eth::v2::TransactionTrace,
    token_store: &StoreGetInt64,
    token_mapping_store: &StoreGetString,
) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut reserves_of = HashMap::new();

    transaction
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .filter(|call| call.address == VAULT_ADDRESS)
        .for_each(|call| {
            if let Some(Settle { token, .. }) = Settle::match_and_decode(call) {
                add_accounted_changes_for_token(
                    &mut reserves_of,
                    &call.storage_changes,
                    token.as_slice(),
                    token_store,
                );
            }
            if let Some(SendTo { token, .. }) = SendTo::match_and_decode(call) {
                add_accounted_changes_for_token(
                    &mut reserves_of,
                    &call.storage_changes,
                    token.as_slice(),
                    token_store,
                );
            }
            if let Some(Erc4626BufferWrapOrUnwrap { params }) =
                Erc4626BufferWrapOrUnwrap::match_and_decode(call)
            {
                let wrapped_token = params.2;
                add_accounted_changes_for_token(
                    &mut reserves_of,
                    &call.storage_changes,
                    wrapped_token.as_slice(),
                    token_store,
                );

                if let Some(underlying_token) = token_mapping_store
                    .get_last(buffer_mapping_key(&wrapped_token))
                    .and_then(|underlying_token| hex::decode(underlying_token).ok())
                {
                    add_accounted_changes_for_token(
                        &mut reserves_of,
                        &call.storage_changes,
                        underlying_token.as_slice(),
                        token_store,
                    );
                }
            }
        });

    reserves_of
        .into_iter()
        .map(|(token, reserve_value)| (token, reserve_value.value))
        .collect()
}

fn add_accounted_changes_for_token(
    reserves_of: &mut HashMap<Vec<u8>, ReserveValue>,
    storage_changes: &[StorageChange],
    token_address: &[u8],
    token_store: &StoreGetInt64,
) {
    for change in storage_changes {
        add_change_if_accounted(reserves_of, change, token_address, token_store);
    }
}

fn add_change_if_accounted(
    reserves_of: &mut HashMap<Vec<u8>, ReserveValue>,
    change: &StorageChange,
    token_address: &[u8],
    token_store: &StoreGetInt64,
) {
    // token_addr -> keccak256(abi.encode(token_address, 8)) as 8 is the order in which reserves of
    // are declared.
    let slot_key = mapping_storage_key_for_address(token_address, RESERVES_OF_SLOT);
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

fn find_underlying_token(call: &eth::v2::Call, wrapped_token: &[u8]) -> Option<Vec<u8>> {
    // Balancer stores the underlying asset for an ERC4626 buffer in `_bufferAssets`:
    // https://github.com/balancer/balancer-v3-monorepo/blob/80fd29ce4eb627139694db7fef5aba355759d303/pkg/vault/contracts/VaultStorage.sol#L163-L164
    //
    // mapping(IERC4626 wrappedToken => address underlyingToken) internal _bufferAssets;
    //
    // wrapped_token -> keccak256(abi.encode(wrapped_token, 14)) as 14 is the order in
    // which _bufferAssets is declared.
    let buffer_asset_key = mapping_storage_key_for_address(wrapped_token, BUFFER_ASSETS_SLOT);
    call.storage_changes
        .iter()
        .find(|change| change.address == VAULT_ADDRESS && change.key == buffer_asset_key)
        .and_then(|change| decode_address_from_storage_word(&change.new_value))
}
