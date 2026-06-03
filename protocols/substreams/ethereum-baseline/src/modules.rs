//! Template for Protocols with singleton contract
//!
//! This template provides a starting point for protocols that follow a singleton
//! pattern. Usually these protocols employ a fixed set of contracts instead of
//! deploying new contracts per component.
//!
//! ## Assumptions
//! - Assumes a single relay contract is enough to simulate all swaps
//! - Assumes any price or liquidity change on a pool is linked to a tvl change
//!
//! ## Alternative Module
//! If your protocol uses individual contracts deployed with a factory to manage
//! components and balances, refer to the `ethereum-template-factory` substream for an
//! appropriate alternative.
//!
//! ## Warning
//! This template provides a general framework for indexing a protocol. However, it is
//! likely that you will need to adapt the steps to suit your specific use case. Use the
//! provided code with care and ensure you fully understand each step before proceeding
//! with your implementation
use crate::{
    abi::{
        b_controller::events::{CreatorFeePctSet, DeployerSet, LiquidityFeePctSet},
        b_factory::events::PoolCreated,
        b_swap::events::Swap,
    },
    pool_factories,
    pool_factories::DeploymentConfig,
};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    prelude::*,
    store::{StoreGet, StoreGetString, StoreSetString},
};
use substreams_ethereum::pb::eth;
use substreams_ethereum::Event;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes,
    contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

const DCI_QUOTE_ENTRYPOINTS: [(&str, [u8; 4]); 4] = [
    ("quoteBuyExactIn(address,uint256)", [0x28, 0x7c, 0x89, 0xa7]),
    ("quoteBuyExactOut(address,uint256)", [0xf3, 0xa8, 0xd7, 0xf2]),
    ("quoteSellExactIn(address,uint256)", [0x0a, 0x64, 0x92, 0x74]),
    ("quoteSellExactOut(address,uint256)", [0xfe, 0x28, 0x97, 0x7c]),
];
const DCI_LENS_ENTRYPOINTS: [(&str, [u8; 4]); 3] = [
    ("reserve(address)", [0xe7, 0x51, 0x79, 0xa4]),
    ("totalBTokens(address)", [0x94, 0x69, 0x51, 0x2b]),
    ("totalReserves(address)", [0x6d, 0x08, 0x00, 0xbc]),
];
const DCI_STAKING_ENTRYPOINTS: [(&str, [u8; 4]); 1] =
    [("getCurrentRate(address)", [0xdc, 0xe7, 0x7d, 0x84])];
const DCI_SAMPLE_AMOUNT_IN: u128 = 1_000_000_000_000_000;

fn dci_quote_calldata(selector: [u8; 4], b_token: &[u8]) -> Vec<u8> {
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(b_token);
    calldata.extend_from_slice(&[0u8; 16]);
    calldata.extend_from_slice(&DCI_SAMPLE_AMOUNT_IN.to_be_bytes());
    calldata
}

fn dci_btoken_calldata(selector: [u8; 4], b_token: &[u8]) -> Vec<u8> {
    let mut calldata = Vec::with_capacity(36);
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(b_token);
    calldata
}

fn maybe_quote_state_update_component_id(log: &eth::v2::Log) -> Option<String> {
    CreatorFeePctSet::match_and_decode(log)
        .map(|event| event.b_token)
        .or_else(|| LiquidityFeePctSet::match_and_decode(log).map(|event| event.b_token))
        .or_else(|| DeployerSet::match_and_decode(log).map(|event| event.b_token))
        .map(|b_token| format!("0x{}", hex::encode(b_token)))
}

/// Find and create all relevant protocol components
///
/// This method maps over blocks and instantiates ProtocolComponents with a unique ids
/// as well as all necessary metadata for routing and encoding.
#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config = serde_qs::from_str(params.as_str())?;
    Ok(BlockTransactionProtocolComponents {
        tx_components: block
            .transactions()
            .filter_map(|tx| {
                let components = tx
                    .logs_with_calls()
                    .filter_map(|(log, call)| {
                        // TODO: ensure this method is implemented correctly
                        pool_factories::maybe_create_component(call.call, log, tx, &config)
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

/// Extracts balance changes per component
///
/// This function parses protocol specific events that incur tvl changes into
/// BalanceDelta structs.
///
/// ## Note:
/// - You only need to account for balances that immediately available as liquidity, e.g. user
///   deposits or accumulated swap fees should not be accounted for.
/// - Take special care if your protocol uses native ETH or your component burns or mints tokens.
/// - You may want to ignore LP tokens if the tvl is covered via regular erc20 tokens.
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
                .flat_map(|(log, _call)| -> Vec<BalanceDelta> {
                    if log.address != config.relay_address {
                        return vec![];
                    }

                    if let Some(event) = PoolCreated::match_and_decode(log) {
                        let component_id =
                            format!("0x{}", hex::encode(&event.b_token_address)).into_bytes();

                        return vec![
                            BalanceDelta {
                                ord: log.ordinal,
                                tx: Some(tx.into()),
                                token: event.b_token_address,
                                delta: event
                                    .total_b_tokens
                                    .to_signed_bytes_be(),
                                component_id: component_id.clone(),
                            },
                            BalanceDelta {
                                ord: log.ordinal,
                                tx: Some(tx.into()),
                                token: event.reserve_address,
                                delta: event
                                    .total_reserves
                                    .to_signed_bytes_be(),
                                component_id,
                            },
                        ];
                    }

                    if let Some(event) = Swap::match_and_decode(log) {
                        let component_id = format!("0x{}", hex::encode(&event.b_token));
                        let Some(reserve) =
                            reserve_store.get_last(format!("reserve:{component_id}"))
                        else {
                            return vec![];
                        };
                        let Ok(reserve) = hex::decode(reserve) else {
                            return vec![];
                        };

                        let b_token_delta = event.b_token_delta.neg();
                        let reserve_delta =
                            event.reserve_delta.neg() - event.total_fee + event.liquidity_fee;
                        let component_id = component_id.into_bytes();

                        return vec![
                            BalanceDelta {
                                ord: log.ordinal,
                                tx: Some(tx.into()),
                                token: event.b_token,
                                delta: b_token_delta.to_signed_bytes_be(),
                                component_id: component_id.clone(),
                            },
                            BalanceDelta {
                                ord: log.ordinal,
                                tx: Some(tx.into()),
                                token: reserve,
                                delta: reserve_delta.to_signed_bytes_be(),
                                component_id,
                            },
                        ];
                    }

                    vec![]
                })
        })
        .collect::<Vec<_>>();

    Ok(BlockBalanceDeltas { balance_deltas: res })
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
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
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
                    builder.add_entity_change(&EntityChanges {
                        component_id: component.id.clone(),
                        attributes: vec![Attribute {
                            name: "balance_owner".to_string(),
                            value: config.relay_address.clone(),
                            change: ChangeType::Creation.into(),
                        }],
                    });
                    let Some(b_token) = component.tokens.first() else {
                        return;
                    };
                    DCI_QUOTE_ENTRYPOINTS
                        .iter()
                        .for_each(|(signature, selector)| {
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                config.relay_address.clone(),
                                signature.to_string(),
                                component.id.clone(),
                                TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: dci_quote_calldata(*selector, b_token),
                                }),
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
                        });
                    DCI_LENS_ENTRYPOINTS
                        .iter()
                        .for_each(|(signature, selector)| {
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                config.relay_address.clone(),
                                signature.to_string(),
                                component.id.clone(),
                                TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: dci_btoken_calldata(*selector, b_token),
                                }),
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
                        });
                    DCI_STAKING_ENTRYPOINTS
                        .iter()
                        .for_each(|(signature, selector)| {
                            let (entrypoint, entrypoint_params) = create_entrypoint(
                                config.relay_address.clone(),
                                signature.to_string(),
                                component.id.clone(),
                                TraceData::Rpc(RpcTraceData {
                                    caller: None,
                                    calldata: dci_btoken_calldata(*selector, b_token),
                                }),
                            );
                            builder.add_entrypoint(&entrypoint);
                            builder.add_entrypoint_params(&entrypoint_params);
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
            let mut contract_changes = InterimContractChange::new(&config.relay_address, false);
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map.values().for_each(|bc| {
                        // track component balance
                        builder.add_balance_change(bc);
                        // Mark this component as updates since we are using manual update tracking
                        // TODO: ensure this covers all cases a component should be marked as
                        let component_id =
                            String::from_utf8(bc.component_id.clone()).expect("bad component id");
                        builder.mark_component_as_updated(&component_id);
                        // track vault contract balance
                        contract_changes
                            .upsert_token_balance(bc.token.as_slice(), bc.balance.as_slice())
                    })
                });
            builder.add_contract_changes(&contract_changes);
        });

    // Some controller operations update quote-relevant pool fields without
    // producing swap balance deltas. Mark those components updated so Tycho
    // refreshes simulation state when the relay storage changes.
    block
        .transactions()
        .for_each(|tx| {
            tx.logs_with_calls()
                .filter_map(|(log, _call)| {
                    if log.address != config.relay_address {
                        return None;
                    }
                    maybe_quote_state_update_component_id(log)
                })
                .for_each(|component_id| {
                    let tx: Transaction = tx.into();
                    let builder = transaction_changes
                        .entry(tx.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(&tx));
                    builder.mark_component_as_updated(&component_id);
                });
        });

    // Extract and insert any storage changes that happened for any of the components.
    extract_contract_changes_builder(
        &block,
        |addr| {
            // we assume that the store holds contract addresses as keys and if it
            // contains a value, that contract is of relevance.
            // TODO: if you have any additional static contracts that need to be indexed,
            //  please add them here.
            addr == config.relay_address
        },
        &mut transaction_changes,
    );

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
