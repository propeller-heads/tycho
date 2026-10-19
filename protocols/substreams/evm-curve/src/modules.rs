use std::collections::{HashMap, HashSet};

use anyhow::Result;
use itertools::Itertools;
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{
        StoreAddBigInt, StoreGet, StoreGetInt64, StoreGetString, StoreNew, StoreSet, StoreSetInt64,
        StoreSetString,
    },
};
use substreams_ethereum::{pb::eth, Function};

use crate::{
    abi,
    abi::set_oracle_implementation::functions::SetOracle,
    consts::ZERO_ADDRESS,
    params::{emit_specific_pools, parse_curve_params},
    pool_changes::emit_eth_deltas,
    pool_factories,
};
use tycho_substreams::{
    attributes::json_deserialize_address_list,
    balances::{extract_balance_deltas_from_tx, store_balance_changes},
    block_storage::get_block_storage_changes,
    contract::extract_contract_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

/// This struct purely exists to spoof the `PartialEq` trait for `Transaction` so we can use it in
///  a later groupby operation.
#[derive(Debug)]
struct TransactionWrapper(Transaction);

impl PartialEq for TransactionWrapper {
    fn eq(&self, other: &Self) -> bool {
        self.0.hash == other.0.hash
    }
}

#[substreams::handlers::map]
// Map all created components and their related entity changes.
pub fn map_components(params: String, block: eth::v2::Block) -> Result<BlockChanges> {
    let curve_params = parse_curve_params(&params)?;
    let changes = block
        .transactions()
        .filter_map(|tx| {
            let mut entity_changes = vec![];
            let mut components = vec![];

            for (log, call) in tx
                .logs_with_calls()
                .filter(|(_, call)| !call.call.state_reverted)
            {
                if let Some((component, mut state)) = pool_factories::address_map(
                    &curve_params,
                    call.call
                        .address
                        .as_slice()
                        .try_into()
                        .ok()?, // this shouldn't fail
                    log,
                    call.call,
                    tx,
                ) {
                    entity_changes.append(&mut state);
                    components.push(component);
                }
            }

            if let Some((component, mut state)) = emit_specific_pools(&params, tx).expect(
                "An unexpected error occurred when parsing params for emitting specific pools",
            ) {
                entity_changes.append(&mut state);
                components.push(component);
            }

            if components.is_empty() {
                None
            } else {
                Some(TransactionChanges {
                    tx: Some(Transaction {
                        hash: tx.hash.clone(),
                        from: tx.from.clone(),
                        to: tx.to.clone(),
                        index: tx.index.into(),
                    }),
                    contract_changes: vec![],
                    entity_changes,
                    component_changes: components,
                    balance_changes: vec![],
                    entrypoints: vec![],
                    entrypoint_params: vec![],
                })
            }
        })
        .collect::<Vec<_>>();

    Ok(BlockChanges { block: None, changes, storage_changes: vec![] })
}

/// Get result `map_components` and stores the created `ProtocolComponent`s with the pool id as the
/// key and tokens as the value
#[substreams::handlers::store]
pub fn store_component_tokens(map: BlockChanges, store: StoreSetString) {
    map.changes
        .iter()
        .flat_map(|tx_changes| &tx_changes.component_changes)
        .for_each(|component| {
            store.set(
                0,
                format!("pool:{0}", component.id),
                &component
                    .tokens
                    .iter()
                    .map(hex::encode)
                    .join(":"),
            );
        });
}

/// Stores contracts required by components, for example LP tokens if they are different from the
/// pool.
/// This is later used to index them with `extract_contract_changes`
#[substreams::handlers::store]
pub fn store_non_component_accounts(map: BlockChanges, store: StoreSetInt64) {
    map.changes
        .iter()
        .flat_map(|tx_changes| &tx_changes.component_changes)
        .for_each(|component| {
            // Crypto pool factory creates LP token separated from the pool, we need to index it so
            // we add it to the store if the new protocol component comes from this factory
            if component.has_attributes(&[
                ("pool_type", "crypto_pool".into()),
                ("factory_name", "crypto_pool_factory".into()),
            ]) {
                let lp_token = component
                    .get_attribute_value("lp_token")
                    .expect("didn't find lp_token attribute");
                store.set(0, hex::encode(lp_token), &1);
            }
        });
}

#[substreams::handlers::store]
pub fn store_set_oracle_components(map: BlockChanges, store: StoreSetInt64) {
    map.changes
        .iter()
        .for_each(|tx_changes| {
            tx_changes
                .component_changes
                .iter()
                .for_each(|pc| {
                    if let Some(use_set_oracle) = pc.get_attribute_value("set_oracle") {
                        if use_set_oracle[0] == 1 {
                            store.set(0, &pc.id, &1);
                        }
                    }
                })
        })
}

/// Since the `PoolBalanceChanged` events administer only deltas, we need to leverage a map and a
///  store to be able to tally up final balances for tokens in a pool.
#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    tokens_store: StoreGetString,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let curve_param = parse_curve_params(&params)?;
    Ok(BlockBalanceDeltas {
        balance_deltas: {
            let mut deltas: Vec<_> = block
                .transactions()
                .flat_map(|tx| {
                    emit_eth_deltas(tx, &tokens_store)
                        .into_iter()
                        .chain(
                            extract_balance_deltas_from_tx(tx, |token, transactor| {
                                let pool_key = format!("pool:0x{}", hex::encode(transactor));
                                if let Some(tokens) = tokens_store.get_last(pool_key) {
                                    let token_id = if token != ZERO_ADDRESS &&
                                        token == curve_param.protocol_params.old_susd
                                    {
                                        hex::encode(curve_param.protocol_params.new_susd)
                                    } else {
                                        hex::encode(token)
                                    };
                                    tokens.split(':').any(|t| t == token_id)
                                } else {
                                    false
                                }
                            })
                            .into_iter()
                            .map(|mut balance| {
                                if balance.token != ZERO_ADDRESS &&
                                    balance.token == curve_param.protocol_params.old_susd
                                {
                                    balance.token = curve_param
                                        .protocol_params
                                        .new_susd
                                        .into();
                                }
                                balance
                            })
                            .collect::<Vec<_>>(),
                        )
                })
                .collect();

            // Keep it consistent with how it's inserted in the store. This step is important
            // because we use a zip on the store deltas and balance deltas later.
            #[allow(clippy::unnecessary_sort_by)]
            deltas.sort_unstable_by(|a, b| a.ord.cmp(&b.ord));

            deltas
        },
    })
}

/// It's significant to include both the `pool_id` and the `token_id` for each balance delta as the
///  store key to ensure that there's a unique balance being tallied for each.
#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    store_balance_changes(deltas, store)
}

/// This is the main map that handles most of the indexing of this substream.
/// Every change is grouped by transaction index via the `transaction_changes`
///  map. Each block of code will extend the `TransactionChanges` struct with the
///  cooresponding changes (balance, component, contract), inserting a new one if it doesn't exist.
///  At the very end, the map can easily be sorted by index to ensure the final
/// `BlockChanges` is ordered by transactions properly.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    grouped_components: BlockChanges,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetString,
    non_component_accounts_store: StoreGetInt64,
    set_oracle_components_store: StoreGetInt64,
    balance_store: StoreDeltas, // Note, this map module is using the `deltas` mode for the store.
) -> Result<BlockChanges> {
    let curve_param = parse_curve_params(&params)?;
    // We merge contract changes by transaction (identified by transaction index) making it easy to
    //  sort them at the very end.
    let mut transaction_changes: HashMap<_, TransactionChanges> = HashMap::new();

    for trx in block.transactions() {
        let tx = Transaction {
            to: trx.to.clone(),
            from: trx.from.clone(),
            hash: trx.hash.clone(),
            index: trx.index.into(),
        };
        let transaction_entry = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChanges {
                tx: Some(tx.clone()),
                contract_changes: vec![],
                component_changes: vec![],
                balance_changes: vec![],
                entrypoints: vec![],
                entity_changes: vec![],
                entrypoint_params: vec![],
            });
        for call in trx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
        {
            if let Some(set_oracle) = SetOracle::match_and_decode(call) {
                if set_oracle_components_store
                    .get_last(format!("0x{0}", hex::encode(&call.address)))
                    .is_some() &&
                    set_oracle.oracle != ZERO_ADDRESS
                {
                    let trace_data = TraceData::Rpc(RpcTraceData {
                        caller: None,
                        calldata: set_oracle.method_id.to_vec(),
                    });
                    let (entrypoint, entrypoint_param) = create_entrypoint(
                        set_oracle.oracle.to_vec(),
                        hex::encode(set_oracle.method_id),
                        format!("0x{0}", hex::encode(&call.address)),
                        trace_data,
                    );
                    transaction_entry
                        .entrypoints
                        .push(entrypoint);
                    transaction_entry
                        .entrypoint_params
                        .push(entrypoint_param);
                }
            }
        }
    }
    // `ProtocolComponents` are gathered with some entity changes from `map_pools_created` which
    // just need a bit of work to  convert into `TransactionChanges`
    grouped_components
        .changes
        .into_iter()
        .for_each(|tx_changes| {
            let tx = tx_changes.tx.as_ref().unwrap();
            let transaction_entry = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChanges {
                    tx: Some(tx.clone()),
                    contract_changes: vec![],
                    component_changes: vec![],
                    balance_changes: vec![],
                    entrypoints: vec![],
                    entity_changes: vec![],
                    entrypoint_params: vec![],
                });

            let mut entrypoints = HashSet::new();
            let mut entrypoint_params = HashSet::new();

            tx_changes
                .component_changes
                .iter()
                .for_each(|component| {
                    let pool_addr = hex::decode(component.id.trim_start_matches("0x"))
                        .unwrap_or_else(|e| panic!("Invalid hex in address: {e}"));
                    let rebase_tokens = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "rebase_tokens")
                        .map(|att| json_deserialize_address_list(&att.value));
                    if let Some(rebase_tokens) = rebase_tokens {
                        rebase_tokens.into_iter().for_each(|r| {
                            let trace_data = TraceData::Rpc(RpcTraceData {
                                caller: Some(pool_addr.clone()),
                                calldata: [
                                    hex::decode("70a08231")
                                        .unwrap()
                                        .as_slice(), // balanceOf(address)
                                    &[0u8; 12],
                                    &pool_addr,
                                ]
                                .concat(),
                            });

                            let (entrypoint, entrypoint_param) = create_entrypoint(
                                r.clone(),
                                "balanceOf".to_string(),
                                component.id.clone(),
                                trace_data,
                            );

                            entrypoints.insert(entrypoint);
                            entrypoint_params.insert(entrypoint_param);
                        });
                    }

                    let base_pool = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "base_pool")
                        .map(|att| {
                            let s = String::from_utf8(att.value.clone())
                                .expect("Invalid UTF-8 in base_pool");
                            hex::decode(s.trim_start_matches("0x"))
                                .expect("Invalid hex bytes in base_pool")
                        });

                    let implementation_idx = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "implementation_idx")
                        .map(|att| BigInt::from_signed_bytes_be(&att.value));
                    if let (Some(base_pool), Some(implementation_idx)) =
                        (base_pool, implementation_idx)
                    {
                        if implementation_idx == BigInt::from(0) {
                            let trace_data = TraceData::Rpc(RpcTraceData {
                                caller: Some(pool_addr.clone()),
                                calldata: hex::decode("bb7b8b80").unwrap(),
                            });
                            let (entrypoint, entrypoint_param) = create_entrypoint(
                                base_pool,
                                "get_virtual_price".to_string(),
                                component.id.clone(),
                                trace_data,
                            );
                            entrypoints.insert(entrypoint);
                            entrypoint_params.insert(entrypoint_param);
                        }
                    }

                    let asset_types: Option<Vec<BigInt>> = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "asset_types")
                        .map(|att| {
                            let value: Vec<String> = serde_json::from_slice(&att.value)
                                .unwrap_or_else(|e| panic!("Failed to decode asset_types: {e}"));

                            value
                                .into_iter()
                                .map(|s| {
                                    let s = s.trim_start_matches("0x");
                                    let bytes = hex::decode(s).unwrap_or_else(|e| {
                                        panic!("Invalid hex in asset_types: {e}")
                                    });
                                    BigInt::from_signed_bytes_be(&bytes)
                                })
                                .collect::<Vec<BigInt>>()
                        });
                    let coins = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "coins")
                        .map(|att| json_deserialize_address_list(&att.value));

                    let oracles = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "oracles")
                        .map(|att| json_deserialize_address_list(&att.value));

                    let method_ids = component
                        .static_att
                        .iter()
                        .find(|att| att.name == "method_ids")
                        .map(|att| {
                            let strs: Vec<String> = serde_json::from_slice(&att.value)
                                .unwrap_or_else(|e| panic!("Failed to decode method_ids: {e}"));

                            strs.into_iter()
                                .map(|s| {
                                    let bytes = hex::decode(s.trim_start_matches("0x"))
                                        .unwrap_or_else(|e| {
                                            panic!("Invalid hex in method_ids: {e}")
                                        });
                                    if bytes.len() != 4 {
                                        panic!("method_id must be 4 bytes, got {}", bytes.len());
                                    }
                                    [bytes[0], bytes[1], bytes[2], bytes[3]]
                                })
                                .collect::<Vec<[u8; 4]>>()
                        });

                    if let Some(coins) = &coins {
                        for coin in coins.iter() {
                            if coin.to_vec() != ZERO_ADDRESS &&
                                coin.to_vec() == curve_param.protocol_params.rusdy
                            {
                                let trace_data = TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: [
                                        hex::decode("fbac3951")
                                            .unwrap()
                                            .as_slice(), // isBlocked(address)
                                        &[0u8; 12],
                                        &pool_addr,
                                    ]
                                    .concat(),
                                });
                                let (entrypoint, entrypoint_param) = create_entrypoint(
                                    curve_param
                                        .protocol_params
                                        .rusdy_blocklist
                                        .to_vec(),
                                    "isBlocked".to_string(),
                                    component.id.clone(),
                                    trace_data,
                                );

                                entrypoints.insert(entrypoint);
                                entrypoint_params.insert(entrypoint_param);
                            }
                            if coin.to_vec() != ZERO_ADDRESS &&
                                coin.to_vec() == curve_param.protocol_params.steth
                            {
                                let trace_data = TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: hex::decode("3f683b6a").unwrap(),
                                });
                                let (entrypoint, entrypoint_param) = create_entrypoint(
                                    coin.clone(),
                                    "isStopped".to_string(),
                                    component.id.clone(),
                                    trace_data,
                                );

                                entrypoints.insert(entrypoint);
                                entrypoint_params.insert(entrypoint_param);
                            }
                        }
                    }
                    if let (Some(asset_types), Some(oracles), Some(method_ids), Some(coins)) =
                        (asset_types, oracles, method_ids, coins)
                    {
                        for (((oracle, method_id), asset_type), coin) in oracles
                            .into_iter()
                            .zip(method_ids.into_iter())
                            .zip(asset_types.into_iter())
                            .zip(coins.into_iter())
                        {
                            if asset_type == BigInt::from(1) {
                                // oracle
                                if oracle == ZERO_ADDRESS {
                                    continue;
                                }

                                let trace_data = TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: method_id.to_vec(),
                                });

                                let (entrypoint, entrypoint_param) = create_entrypoint(
                                    oracle,
                                    hex::encode(method_id),
                                    component.id.clone(),
                                    trace_data,
                                );

                                entrypoints.insert(entrypoint);
                                entrypoint_params.insert(entrypoint_param);
                            }
                            if asset_type == BigInt::from(3) {
                                let token_decimals = get_token_decimals(&coin);
                                let trace_token_amount = match token_decimals {
                                    Some(decimals) => {
                                        let base = BigInt::from(10);
                                        base.pow(
                                            decimals
                                                .to_string()
                                                .parse::<u32>()
                                                .unwrap_or(18),
                                        )
                                    }
                                    None => BigInt::from(1),
                                };
                                // ERC4626
                                let trace_data = TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: build_entrypoint_calldata(
                                        "07a2d13a",
                                        &trace_token_amount,
                                    ), // convertToAssets
                                });

                                let (entrypoint, entrypoint_param) = create_entrypoint(
                                    coin,
                                    "convertToAssets".to_string(),
                                    component.id.clone(),
                                    trace_data,
                                );

                                entrypoints.insert(entrypoint);
                                entrypoint_params.insert(entrypoint_param);
                            }
                        }
                    }
                });

            transaction_entry
                .component_changes
                .extend(tx_changes.component_changes);
            transaction_entry
                .entity_changes
                .extend(tx_changes.entity_changes);

            transaction_entry
                .entrypoints
                .extend(entrypoints);
            transaction_entry
                .entrypoint_params
                .extend(entrypoint_params);
        });

    // Balance changes are gathered by the `StoreDelta` based on `TokenExchange`, etc. creating
    //  `BalanceDeltas`. We essentially just process the changes that occurred to the `store` this
    //  block. Then, these balance changes are merged onto the existing map of tx contract changes,
    //  inserting a new one if it doesn't exist.
    balance_store
        .deltas
        .into_iter()
        .zip(deltas.balance_deltas)
        .map(|(store_delta, balance_delta)| {
            let new_value_string = String::from_utf8(store_delta.new_value)
                .unwrap()
                .to_string();
            (
                balance_delta.tx.unwrap(),
                BalanceChange {
                    token: balance_delta.token,
                    balance: BigInt::try_from(new_value_string)
                        .unwrap()
                        .to_signed_bytes_be(),
                    component_id: format!(
                        "0x{}",
                        String::from_utf8(balance_delta.component_id).unwrap()
                    )
                    .into(),
                },
            )
        })
        // We need to group the balance changes by tx hash for the `TransactionChanges` agg
        .chunk_by(|(tx, _)| TransactionWrapper(tx.clone()))
        .into_iter()
        .for_each(|(tx_wrapped, group)| {
            let tx = tx_wrapped.0;

            transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChanges {
                    tx: Some(tx.clone()),
                    contract_changes: vec![],
                    component_changes: vec![],
                    balance_changes: vec![],
                    entrypoints: vec![],
                    entity_changes: vec![],
                    entrypoint_params: vec![],
                })
                .balance_changes
                .extend(group.map(|(_, change)| change));
        });

    // General helper for extracting contract changes. Uses block, our component store which holds
    //  all of our tracked deployed pool addresses, and the map of tx contract changes which we
    //  output into for final processing later.
    extract_contract_changes(
        &block,
        |addr| {
            components_store
                .get_last(format!("pool:0x{0}", hex::encode(addr)))
                .is_some() ||
                non_component_accounts_store
                    .get_last(hex::encode(addr))
                    .is_some() ||
                curve_param
                    .contracts_to_index()
                    .contains(
                        addr.try_into()
                            .expect("address should be 20 bytes long"),
                    )
        },
        &mut transaction_changes,
    );

    for change in transaction_changes.values_mut() {
        for balance_change in change.balance_changes.iter_mut() {
            replace_eth_address(&mut balance_change.token);
        }

        for component_change in change.component_changes.iter_mut() {
            for token in component_change.tokens.iter_mut() {
                replace_eth_address(token);
            }
        }
    }

    let block_storage_changes = get_block_storage_changes(&block);

    // Process all `transaction_changes` for final output in the `BlockChanges`,
    //  sorted by transaction index (the key).
    Ok(BlockChanges {
        block: Some(Block {
            number: block.number,
            hash: block.hash.clone(),
            parent_hash: block
                .header
                .as_ref()
                .expect("Block header not present")
                .parent_hash
                .clone(),
            ts: block.timestamp_seconds(),
        }),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, change)| {
                if change.contract_changes.is_empty() &&
                    change.component_changes.is_empty() &&
                    change.balance_changes.is_empty()
                {
                    None
                } else {
                    Some(change)
                }
            })
            .collect::<Vec<_>>(),
        storage_changes: block_storage_changes,
    })
}

fn replace_eth_address(token: &mut Vec<u8>) {
    let eth_address = [238u8; 20]; // 0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
    if *token == eth_address {
        *token = [0u8; 20].to_vec();
    }
}

fn build_entrypoint_calldata(sig: &str, amount: &BigInt) -> Vec<u8> {
    let mut entrypoint_calldata = hex::decode(sig).unwrap();
    let amount_bytes = amount.to_bytes_be().1;
    let mut padded_amount = vec![0u8; 32];
    let start_pos = 32 - amount_bytes.len();
    padded_amount[start_pos..].copy_from_slice(&amount_bytes);
    entrypoint_calldata.extend_from_slice(&padded_amount);
    entrypoint_calldata
}

fn get_token_decimals(wrapped_token: &[u8]) -> Option<BigInt> {
    abi::erc20::functions::Decimals {}.call(wrapped_token.to_owned())
}
