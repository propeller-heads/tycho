use crate::{
    abi::party_pool,
    params::{decode_addrs, encode_addr, encode_addrs, Params},
    pool_factories,
};
use anyhow::Result;
use std::collections::HashMap;
use substreams::{pb::substreams::StoreDeltas, prelude::*};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    balances::aggregate_balances_changes, contract::extract_contract_changes_builder, prelude::*,
};

/// Find and create all relevant protocol components
///
/// This method maps over blocks and instantiates ProtocolComponents with a unique ids
/// as well as all necessary metadata for routing and encoding.
#[substreams::handlers::map]
fn map_protocol_components(
    param_string: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let params = Params::parse(&param_string)?;
    Ok(BlockTransactionProtocolComponents {
        tx_components: block
            .transactions()
            .filter_map(|tx| {
                let components = tx
                    .logs_with_calls()
                    .filter_map(|(log, call)| {
                        pool_factories::maybe_create_component(&params, call.call, log, tx)
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

/// Stores all protocol components.
#[substreams::handlers::store]
fn store_protocol_components(
    map_protocol_components: BlockTransactionProtocolComponents,
    store: StoreSetRaw,
) {
    map_protocol_components
        .tx_components
        .into_iter()
        .for_each(|tx_pc| {
            tx_pc
                .components
                .into_iter()
                .for_each(|pc| {
                    // Assumes that the component id is a hex encoded contract address
                    let key = pc.id.clone();
                    // we store the components tokens
                    let val = encode_addrs(&pc.tokens);
                    store.set(0, key, &val);
                })
        });
}

/// Records killed pool addresses so downstream modules can skip them.
#[substreams::handlers::store]
fn store_killed_components(
    map_killed_components: BlockTransactionProtocolComponents,
    store: StoreSetInt64,
) {
    map_killed_components
        .tx_components
        .into_iter()
        .for_each(|tx_pc| {
            tx_pc
                .components
                .into_iter()
                .for_each(|pc| {
                    store.set(0, pc.id, &1);
                })
        });
}

/// Tracks killed pools that can no longer swap
#[substreams::handlers::map]
fn map_killed_components(
    block: eth::v2::Block,
    store: StoreGetRaw,
) -> Result<BlockTransactionProtocolComponents> {
    Ok(BlockTransactionProtocolComponents {
        tx_components: block
            .transactions()
            .filter_map(|tx| {
                let components = tx
                    .logs_with_calls()
                    .filter_map(|(log, _call)| {
                        party_pool::events::Killed::match_and_decode(log).and_then(|_event| {
                            let from_addr = encode_addr(&log.address);
                            if store.get_last(&from_addr).is_some() {
                                let mut pc = ProtocolComponent::new(&from_addr);
                                pc.change = ChangeType::Deletion.into();
                                Some(pc)
                            } else {
                                None
                            }
                        })
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

/// Tracks TVL changes per component using PartyPool events.
///
/// Protocol fees sit in the pool until being collected but have separate accounting
/// and do not contribute to TVL.
#[substreams::handlers::map]
fn map_relative_component_balance(
    block: eth::v2::Block,
    store: StoreGetString,
    killed_store: StoreGetInt64,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let mut deltas: Vec<BalanceDelta> = Vec::new();

    for log in block.logs() {
        let pool_addr = encode_addr(log.address());
        // Short circuit if the address doesn't match any of our pools
        let Some(tokens_str) = store.get_last(&pool_addr) else { continue };
        // Short circuit if the pool has been killed
        if killed_store
            .get_last(&pool_addr)
            .is_some()
        {
            continue;
        }
        let component_tokens = decode_addrs(&tokens_str)?;
        let component_id = pool_addr.as_bytes().to_vec();
        let tx = log.receipt.transaction;
        let ord = log.ordinal();

        let mut push = |token: Vec<u8>, delta: substreams::scalar::BigInt| {
            if !delta.is_zero() {
                deltas.push(BalanceDelta {
                    ord,
                    tx: Some(tx.into()),
                    token,
                    delta: delta.to_signed_bytes_be(),
                    component_id: component_id.clone(),
                });
            }
        };

        if let Some(ev) = party_pool::events::Mint::match_and_decode(log) {
            // Basket mint deposits some of every token
            for (token, amount) in component_tokens
                .iter()
                .zip(ev.amounts.into_iter())
            {
                push(token.clone(), amount);
            }
        } else if let Some(ev) = party_pool::events::Burn::match_and_decode(log) {
            // Basket burn withdraws some of every token
            for (token, amount) in component_tokens
                .iter()
                .zip(ev.amounts.into_iter())
            {
                push(token.clone(), amount.neg());
            }
        } else if let Some(ev) = party_pool::events::Swap::match_and_decode(log) {
            // Fee-on-output: the full input enters the pool. The protocol fee is
            // taken from the output token and moved to the protocol-fee ledger
            // (excluded from TVL), so the LP-owned reserve loses the net output
            // plus the protocol fee. The LP fee stays in the reserve.
            push(ev.token_in, ev.amount_in);
            push(ev.token_out, (ev.amount_out + ev.protocol_fee).neg());
        } else if let Some(ev) = party_pool::events::SwapMint::match_and_decode(log) {
            // Excludes the protocol fee taken the from input amount
            push(ev.token_in, ev.amount_in - ev.protocol_fee);
        } else if let Some(ev) = party_pool::events::BurnSwap::match_and_decode(log) {
            // The output amount is what the user receives, but the pool TVL also
            // loses the protocol fee.
            push(ev.token_out, (ev.amount_out + ev.protocol_fee).neg());
        }
    }

    Ok(BlockBalanceDeltas { balance_deltas: deltas })
}

/// Aggregates relative balances values into absolute values
///
/// Aggregate the relative balances in an additive store since tycho-indexer expects
/// absolute balance inputs.
///
/// ## Note:
/// This method should usually not require any changes.
#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
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
fn map_protocol_changes(
    param_string: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    killed_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetString,
    balance_store: StoreDeltas, // Note, this map module is using the `deltas` mode for the store.
) -> Result<BlockChanges> {
    let params = Params::parse(&param_string)?;

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

    // We mark killed components with a `killed` dynamic attribute, then
    // tycho-simulation uses a stream filter to remove the pool. See
    // `liquidityparty_killed_pools_filter` in protocol_stream_processor.rs
    killed_components
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
                    builder.add_entity_change(&EntityChanges {
                        component_id: component.id.clone(),
                        attributes: vec![Attribute {
                            name: "killed".to_string(),
                            value: vec![1u8],
                            change: ChangeType::Update.into(),
                        }],
                    });
                });
        });

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

    // Extract and insert any storage changes that happened for any of the components.
    extract_contract_changes_builder(
        &block,
        |addr| {
            let addr_str = encode_addr(addr);
            // we assume that the store holds contract addresses as keys and if it
            // contains a value, that contract is of relevance.
            components_store
                .get_last(addr_str)
                .is_some() ||
                addr == params.extra_impl1.as_slice() ||
                addr == params.extra_impl2.as_slice() ||
                addr == params.planner.as_slice() ||
                addr == params.info.as_slice()
        },
        &mut transaction_changes,
    );

    // Process all `transaction_changes` for final output in the `BlockChanges`,
    //  sorted by transaction index (the key).
    let tx_changes = transaction_changes
        .drain()
        .filter_map(|(_, builder)| builder.build())
        .collect::<Vec<_>>();

    Ok(BlockChanges { block: Some((&block).into()), changes: tx_changes, storage_changes: vec![] })
}
