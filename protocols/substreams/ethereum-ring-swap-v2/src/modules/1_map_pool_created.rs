use std::str::FromStr;

use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::{log, prelude::BigInt};
use substreams_ethereum::{
    pb::eth::{rpc::RpcResponse, v2 as eth},
    rpc::RpcBatch,
};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::{erc20, factory::events::PairCreated, few_wrapped_token};

use tycho_substreams::prelude::*;

#[derive(Debug, Deserialize)]
struct Params {
    factory_address: String,
    protocol_type_name: String,
}

#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionChanges> = vec![];

    let params: Params = serde_qs::from_str(params.as_str()).expect("Unable to deserialize params");

    get_pools(&block, &mut new_pools, &params);

    let tycho_block: Block = (&block).into();

    Ok(BlockChanges { block: Some(tycho_block), changes: new_pools })
}

fn get_pools(block: &eth::Block, new_pools: &mut Vec<TransactionChanges>, params: &Params) {
    let factory_address = Address::from_str(&params.factory_address).unwrap();

    // Extract new pools from PairCreated events
    let mut on_pair_created = |event: PairCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        let Some(tokens) = resolve_ring_tokens(&event) else {
            log::info!(
                "Skipping pool {} because its Ring token metadata could not be resolved",
                event.pair.to_hex()
            );
            return;
        };

        new_pools.push(TransactionChanges {
            tx: Some(tycho_tx.clone()),
            contract_changes: vec![],
            entity_changes: vec![EntityChanges {
                component_id: event.pair.to_hex(),
                attributes: vec![
                    Attribute {
                        name: "reserve0".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "reserve1".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.pair.to_hex(),
                tokens: tokens.component_tokens.clone(),
                contracts: vec![],
                static_att: static_attributes(&event, factory_address, &tokens),
                change: i32::from(ChangeType::Creation),
                protocol_type: Some(ProtocolType {
                    name: params.protocol_type_name.to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
                tx: Some(tycho_tx),
            }],
            balance_changes: vec![
                BalanceChange {
                    token: tokens.component_tokens[0].clone(),
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event.pair.to_hex().as_bytes().to_vec(),
                },
                BalanceChange {
                    token: tokens.component_tokens[1].clone(),
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event.pair.to_hex().as_bytes().to_vec(),
                },
            ],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![factory_address]);

    eh.on::<PairCreated, _>(&mut on_pair_created);
    eh.handle_events();
}

fn static_attributes(
    event: &PairCreated,
    factory_address: Address,
    tokens: &RingTokens,
) -> Vec<Attribute> {
    vec![
        // Trading Fee is hardcoded to 0.3%, saved as int in bps (basis points)
        Attribute {
            name: "fee".to_string(),
            value: BigInt::from(30).to_signed_bytes_be(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "pool_address".to_string(),
            value: event.pair.clone(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "pool_factory".to_string(),
            value: factory_address.as_bytes().to_vec(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "fw_token0".to_string(),
            value: tokens.fw_token0.clone(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "fw_token1".to_string(),
            value: tokens.fw_token1.clone(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "underlying_token0".to_string(),
            value: tokens.underlying_token0.clone(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "underlying_token1".to_string(),
            value: tokens.underlying_token1.clone(),
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "fw_decimals0".to_string(),
            value: vec![tokens.fw_decimals0],
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "fw_decimals1".to_string(),
            value: vec![tokens.fw_decimals1],
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "underlying_decimals0".to_string(),
            value: vec![tokens.underlying_decimals0],
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "underlying_decimals1".to_string(),
            value: vec![tokens.underlying_decimals1],
            change: ChangeType::Creation.into(),
        },
        Attribute {
            name: "reserves_inverted".to_string(),
            value: if tokens.reserves_inverted { vec![1] } else { vec![0] },
            change: ChangeType::Creation.into(),
        },
    ]
}

struct RingTokens {
    component_tokens: Vec<Vec<u8>>,
    fw_token0: Vec<u8>,
    fw_token1: Vec<u8>,
    underlying_token0: Vec<u8>,
    underlying_token1: Vec<u8>,
    fw_decimals0: u8,
    fw_decimals1: u8,
    underlying_decimals0: u8,
    underlying_decimals1: u8,
    reserves_inverted: bool,
}

/// Resolves the underlying ERC-20 metadata of a Ring pair.
///
/// Ring pairs hold FewTokens (wrapped ERC-20s), but components are exposed to solvers with the
/// underlying ERC-20s as tokens. Metadata is fetched with two batched eth_calls: one against both
/// FewTokens (token() + decimals()), and one against the resolved underlying tokens (decimals(),
/// only known once the first batch returns). Returns None when the pair tokens are not FewTokens
/// or any call fails.
fn resolve_ring_tokens(event: &PairCreated) -> Option<RingTokens> {
    let fw_responses = RpcBatch::new()
        .add(few_wrapped_token::functions::Token {}, event.token0.clone())
        .add(few_wrapped_token::functions::Token {}, event.token1.clone())
        .add(erc20::functions::Decimals {}, event.token0.clone())
        .add(erc20::functions::Decimals {}, event.token1.clone())
        .execute()
        .ok()?
        .responses;

    let underlying_token0 = decode_underlying_token(fw_responses.first()?)?;
    let underlying_token1 = decode_underlying_token(fw_responses.get(1)?)?;
    let fw_decimals0 = decode_decimals(fw_responses.get(2)?)?;
    let fw_decimals1 = decode_decimals(fw_responses.get(3)?)?;

    let underlying_responses = RpcBatch::new()
        .add(erc20::functions::Decimals {}, underlying_token0.clone())
        .add(erc20::functions::Decimals {}, underlying_token1.clone())
        .execute()
        .ok()?
        .responses;

    let underlying_decimals0 = decode_decimals(underlying_responses.first()?)?;
    let underlying_decimals1 = decode_decimals(underlying_responses.get(1)?)?;

    // Components expose the underlying tokens sorted by address, matching the UniswapV2 token
    // order convention downstream simulation relies on. The underlying order can differ from the
    // FewToken order of the pair, in which case reserves must be swapped as well.
    let reserves_inverted = underlying_token1 < underlying_token0;
    let component_tokens = if reserves_inverted {
        vec![underlying_token1.clone(), underlying_token0.clone()]
    } else {
        vec![underlying_token0.clone(), underlying_token1.clone()]
    };

    Some(RingTokens {
        component_tokens,
        fw_token0: event.token0.clone(),
        fw_token1: event.token1.clone(),
        underlying_token0,
        underlying_token1,
        fw_decimals0,
        fw_decimals1,
        underlying_decimals0,
        underlying_decimals1,
        reserves_inverted,
    })
}

fn decode_underlying_token(response: &RpcResponse) -> Option<Vec<u8>> {
    if response.failed {
        return None;
    }
    RpcBatch::decode::<_, few_wrapped_token::functions::Token>(response)
}

fn decode_decimals(response: &RpcResponse) -> Option<u8> {
    if response.failed {
        return None;
    }
    let decimals: BigInt = RpcBatch::decode::<_, erc20::functions::Decimals>(response)?;
    // decimals() is uint8 in the ABI, but a non-conforming token can return any 32-byte word;
    // reject values that do not fit u8 instead of panicking.
    decimals.to_string().parse::<u8>().ok()
}
