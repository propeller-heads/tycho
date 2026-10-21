use crate::{
    abi::erc4626::{
        self,
        events::{Deposit, Withdraw},
    },
    pools::PoolParams,
};
use anyhow::Result;
use itertools::Itertools;
use std::{
    collections::{HashMap, HashSet},
    iter::zip,
};
use substreams::{
    pb::substreams::{store_delta::Operation, StoreDeltas},
    prelude::*,
};
use substreams_ethereum::{pb::eth, Event};
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

/// Find and create all relevant protocol components
///
/// This method maps over blocks and instantiates ProtocolComponents with a unique ids
/// as well as all necessary metadata for routing and encoding.
/// Filter configured pools to the current block/tx and build their protocol components
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let pool_params = PoolParams::parse_params(params.as_str())?;

    let pools_for_block: Vec<&PoolParams> = pool_params
        .iter()
        .filter(|p| p.block == block.number)
        .collect();

    if pools_for_block.is_empty() {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] });
    }

    let mut pools_by_tx: HashMap<&str, Vec<&PoolParams>> = HashMap::new();
    for p in &pools_for_block {
        pools_by_tx
            .entry(p.tx_hash.as_str())
            .or_default()
            .push(*p);
    }

    let tx_components: Vec<TransactionProtocolComponents> = block
        .transactions()
        .filter_map(|tx_trace| {
            let encoded_hash = hex::encode(&tx_trace.hash);

            let pools = pools_by_tx
                .get(encoded_hash.as_str())
                .filter(|p| !p.is_empty())?;

            let tx = Transaction {
                hash: tx_trace.hash.clone(),
                from: tx_trace.from.clone(),
                to: tx_trace.to.clone(),
                index: tx_trace.index as u64,
            };

            let mut tx_pc = TransactionProtocolComponents { tx: Some(tx), components: Vec::new() };

            for pool in pools {
                tx_pc
                    .components
                    .push(ProtocolComponent {
                        id: format!("0x{}", pool.address),
                        tokens: vec![
                            hex::decode(&pool.asset).expect("invalid asset hex"), // asset
                            hex::decode(&pool.address).expect("invalid address hex"), // share
                        ],
                        contracts: vec![], /* no on-chain state tracked here; DCI entrypoints
                                            * supply needed state */
                        static_att: zip(
                            pool.static_attribute_keys
                                .clone()
                                .unwrap_or_default()
                                .into_iter(),
                            pool.static_attribute_vals
                                .clone()
                                .unwrap_or_default()
                                .into_iter(),
                        )
                        .map(|(key, value)| Attribute {
                            name: key,
                            value: value.into(),
                            change: ChangeType::Creation.into(),
                        })
                        .collect(),
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "erc4626".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Custom.into(),
                        }),
                    });
            }

            Some(tx_pc)
        })
        .collect();

    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// Get result `map_components` and stores the created `ProtocolComponent`s with the pool id as the
/// key and tokens as the value
#[substreams::handlers::store]
pub fn store_component_tokens(map: BlockTransactionProtocolComponents, store: StoreSetString) {
    map.tx_components
        .iter()
        .flat_map(|tx_pc| &tx_pc.components)
        .for_each(|component| {
            store.set(
                0,
                format!("Pool:{0}", component.id),
                &component
                    .tokens
                    .iter()
                    .map(hex::encode)
                    .join(":"),
            );
        });
}

#[substreams::handlers::map]
pub fn map_relative_balances(
    block: eth::v2::Block,
    tokens_store: StoreGetString,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let mut balance_deltas = vec![];
    for tx in block.transactions() {
        for (log, _) in tx.logs_with_calls() {
            let pool_id = format!("Pool:{}", log.address.to_hex());
            let Some(pool_tokens_str) = tokens_store.get_last(&pool_id) else {
                continue;
            };
            let pool_tokens: Vec<Vec<u8>> = pool_tokens_str
                .split(':')
                .map(hex::decode)
                .collect::<Result<Vec<_>, _>>()?;

            // Track only underlying asset deltas for TVL (totalAssets * asset_price); share-token
            // deltas are skipped to avoid skewing protocol TVL calculations.
            if let Some(deposit) = Deposit::match_and_decode(log) {
                balance_deltas.push(BalanceDelta {
                    ord: log.ordinal,
                    tx: Some(tx.into()),
                    token: pool_tokens[0].clone(),
                    delta: deposit.assets.to_signed_bytes_be(),
                    component_id: log
                        .address
                        .clone()
                        .to_hex()
                        .as_bytes()
                        .to_vec(),
                });
            }
            if let Some(withdraw) = Withdraw::match_and_decode(log) {
                balance_deltas.push(BalanceDelta {
                    ord: log.ordinal,
                    tx: Some(tx.into()),
                    token: pool_tokens[0].clone(),
                    delta: withdraw
                        .assets
                        .neg()
                        .to_signed_bytes_be(),
                    component_id: log
                        .address
                        .clone()
                        .to_hex()
                        .as_bytes()
                        .to_vec(),
                });
            }
        }
    }

    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Aggregates relative balances values into absolute values
///
/// Aggregate the relative balances in an additive store since tycho-indexer expects
/// absolute balance inputs.
///
/// ## Note:
/// This method should usually not require any changes.
#[substreams::handlers::store]
pub fn store_component_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

/// Record the first deposit/withdraw interaction per protocol component so we can
/// emit entrypoints only once; uses delta mode to ensure entrypoints are added
/// only on the initial interaction.
#[substreams::handlers::store]
pub fn store_component_first_interaction(
    deltas: BlockBalanceDeltas,
    store: StoreSetIfNotExistsInt64,
) {
    for delta in deltas.balance_deltas {
        let component_id =
            String::from_utf8(delta.component_id.clone()).expect("component_id is not valid utf-8");
        store.set_if_not_exists(0, format!("first:{}", component_id), &1);
    }
}

/// Aggregates protocol components and balance changes by transaction.
///
/// This is the main method that will aggregate all changes as well as extract all
/// relevant contract storage deltas.
///
/// ## Note:
/// You may have to change this method if your components have any default dynamic
/// attributes, or if you need any additional static contracts indexed.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    tokens_store: StoreGetString,
    balance_store: StoreDeltas,
    deltas: BlockBalanceDeltas,
    first_interaction_store: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    // We merge contract changes by transaction (identified by transaction index)
    // making it easy to sort them at the very end.
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // Aggregate newly created components per tx
    new_components
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
                    builder.add_protocol_component(component);
                });
        });

    let mut emitted_pools: HashSet<String> = HashSet::new();

    let first_pools: HashSet<String> = first_interaction_store
        .deltas
        .iter()
        .filter(|delta| delta.operation == Operation::Create as i32)
        .filter_map(|delta| {
            delta
                .key
                .strip_prefix("first:")
                .map(|id| id.to_string())
        })
        .collect();

    if !first_pools.is_empty() {
        for tx in block.transactions() {
            for (log, _call) in tx.logs_with_calls().filter(|(log, _)| {
                if !first_pools.contains(&log.address.to_hex()) {
                    return false;
                }
                tokens_store
                    .get_last(format!("Pool:{}", log.address.to_hex()))
                    .is_some()
            }) {
                let (assets, shares, user) = if let Some(ev) = Deposit::match_and_decode(log) {
                    (ev.assets, ev.shares, ev.sender)
                } else if let Some(ev) = Withdraw::match_and_decode(log) {
                    (ev.assets, ev.shares, ev.sender)
                } else {
                    continue;
                };

                if !emitted_pools.insert(log.address.to_hex()) {
                    continue;
                }

                let tx_meta: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(tx_meta.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&tx_meta));

                let mut add_entrypoint = |signature: &str, calldata: Vec<u8>| {
                    let trace_data = TraceData::Rpc(RpcTraceData { caller: None, calldata });

                    let (ep, ep_param) = create_entrypoint(
                        log.address.clone(),
                        signature.to_string(),
                        log.address.to_hex(),
                        trace_data,
                    );

                    builder.add_entrypoint(&ep);
                    builder.add_entrypoint_params(&ep_param);
                };
                add_entrypoint("totalSupply()", erc4626::functions::TotalSupply {}.encode());
                add_entrypoint(
                    "convertToShares(uint256)",
                    erc4626::functions::ConvertToShares { assets: assets.clone() }.encode(),
                );
                add_entrypoint(
                    "convertToAssets(uint256)",
                    erc4626::functions::ConvertToAssets { shares: shares.clone() }.encode(),
                );
                add_entrypoint(
                    "maxDeposit(address)",
                    erc4626::functions::MaxDeposit { receiver: user.clone() }.encode(),
                );
                add_entrypoint(
                    "maxRedeem(address)",
                    erc4626::functions::MaxRedeem { owner: user.clone() }.encode(),
                );
            }
        }
    }

    // Aggregate absolute balances per transaction.
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
