use std::str::FromStr;

use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::{log, prelude::BigInt};
use substreams_ethereum::{
    pb::eth::{rpc::RpcResponse, v2 as eth},
    rpc::RpcBatch,
};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::{factory::events::PairCreated, few_factory, few_wrapped_token};

use tycho_substreams::prelude::*;

const PROTOCOL_TYPE_NAME: &str = "ring_swap_v2_pool";

#[derive(Debug, Deserialize)]
struct Params {
    factory_address: String,
    few_factory_address: String,
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

    Ok(BlockChanges { block: Some(tycho_block), changes: new_pools, storage_changes: vec![] })
}

fn get_pools(block: &eth::Block, new_pools: &mut Vec<TransactionChanges>, params: &Params) {
    let factory_address = Address::from_str(&params.factory_address).unwrap();
    let few_factory_address = Address::from_str(&params.few_factory_address).unwrap();

    // Extract new pools from PairCreated events
    let mut on_pair_created = |event: PairCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        let Some(tokens) = resolve_ring_tokens(&event, few_factory_address) else {
            log::info!(
                "Skipping pool {} because its Ring token metadata could not be resolved",
                event.pair.to_hex()
            );
            return;
        };

        new_pools.push(TransactionChanges {
            tx: Some(tycho_tx),
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
                // Wrapper backing changes are folded into component balance changes by the Ring
                // Substreams package, so no DCI-tracked contracts are required here.
                contracts: vec![],
                static_att: static_attributes(&event, &tokens),
                change: i32::from(ChangeType::Creation),
                protocol_type: Some(ProtocolType {
                    name: PROTOCOL_TYPE_NAME.to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
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
            entrypoints: vec![],
            entrypoint_params: vec![],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![factory_address]);

    eh.on::<PairCreated, _>(&mut on_pair_created);
    eh.handle_events();
}

fn static_attributes(event: &PairCreated, tokens: &RingTokens) -> Vec<Attribute> {
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
    reserves_inverted: bool,
}

/// Resolves the underlying ERC-20 metadata of a Ring pair.
///
/// Ring pairs hold FewTokens (wrapped ERC-20s), but components are exposed to solvers with the
/// underlying ERC-20s as tokens. Metadata is fetched with two batched eth_calls: one against both
/// pair tokens (token()), and one against FewFactory for the resolved underlying tokens. Returns
/// None when the pair tokens are not official FewFactory wrappers or any call fails.
fn resolve_ring_tokens(event: &PairCreated, few_factory_address: Address) -> Option<RingTokens> {
    let fw_responses = RpcBatch::new()
        .add(few_wrapped_token::functions::Token {}, event.token0.clone())
        .add(few_wrapped_token::functions::Token {}, event.token1.clone())
        .execute()
        .ok()?
        .responses;

    let underlying_token0 = decode_underlying_token(fw_responses.first()?)?;
    let underlying_token1 = decode_underlying_token(fw_responses.get(1)?)?;

    let underlying_responses = RpcBatch::new()
        .add(
            few_factory::functions::GetWrappedToken { original_token: underlying_token0.clone() },
            few_factory_address.as_bytes().to_vec(),
        )
        .add(
            few_factory::functions::GetWrappedToken { original_token: underlying_token1.clone() },
            few_factory_address.as_bytes().to_vec(),
        )
        .execute()
        .ok()?
        .responses;

    let official_fw_token0 = decode_official_wrapped_token(underlying_responses.first()?)?;
    let official_fw_token1 = decode_official_wrapped_token(underlying_responses.get(1)?)?;

    if !official_wrappers_match(
        &event.token0,
        &event.token1,
        &official_fw_token0,
        &official_fw_token1,
    ) {
        log::info!(
            "Skipping pool {} because at least one token is not the official FewFactory wrapper",
            event.pair.to_hex()
        );
        return None;
    }

    // Components expose the underlying tokens sorted by address, matching the UniswapV2 token
    // order convention downstream simulation relies on. The underlying order can differ from the
    // FewToken order of the pair, in which case reserves must be swapped as well.
    let Some((component_tokens, reserves_inverted)) =
        component_token_order(&underlying_token0, &underlying_token1)
    else {
        log::info!(
            "Skipping pool {} because both official FewTokens resolve to the same underlying token {}",
            event.pair.to_hex(),
            underlying_token0.to_hex()
        );
        return None;
    };

    Some(RingTokens {
        component_tokens,
        fw_token0: event.token0.clone(),
        fw_token1: event.token1.clone(),
        underlying_token0,
        underlying_token1,
        reserves_inverted,
    })
}

fn decode_underlying_token(response: &RpcResponse) -> Option<Vec<u8>> {
    if response.failed {
        return None;
    }
    RpcBatch::decode::<_, few_wrapped_token::functions::Token>(response)
}

fn decode_official_wrapped_token(response: &RpcResponse) -> Option<Vec<u8>> {
    if response.failed {
        return None;
    }
    RpcBatch::decode::<_, few_factory::functions::GetWrappedToken>(response)
}

fn official_wrappers_match(
    fw_token0: &[u8],
    fw_token1: &[u8],
    official_fw_token0: &[u8],
    official_fw_token1: &[u8],
) -> bool {
    fw_token0 == official_fw_token0 && fw_token1 == official_fw_token1
}

fn component_token_order(
    underlying_token0: &[u8],
    underlying_token1: &[u8],
) -> Option<(Vec<Vec<u8>>, bool)> {
    match underlying_token0.cmp(underlying_token1) {
        std::cmp::Ordering::Less => {
            Some((vec![underlying_token0.to_vec(), underlying_token1.to_vec()], false))
        }
        std::cmp::Ordering::Greater => {
            Some((vec![underlying_token1.to_vec(), underlying_token0.to_vec()], true))
        }
        std::cmp::Ordering::Equal => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_wrappers_match_accepts_factory_registered_tokens() {
        assert!(official_wrappers_match(&[1], &[2], &[1], &[2]));
    }

    #[test]
    fn official_wrappers_match_rejects_spoofed_wrapper() {
        assert!(!official_wrappers_match(&[9], &[2], &[1], &[2]));
    }

    #[test]
    fn component_token_order_keeps_sorted_underlyings() {
        let (tokens, reserves_inverted) = component_token_order(&[1], &[2]).unwrap();

        assert_eq!(tokens, vec![vec![1], vec![2]]);
        assert!(!reserves_inverted);
    }

    #[test]
    fn component_token_order_inverts_unsorted_underlyings() {
        let (tokens, reserves_inverted) = component_token_order(&[2], &[1]).unwrap();

        assert_eq!(tokens, vec![vec![1], vec![2]]);
        assert!(reserves_inverted);
    }

    #[test]
    fn component_token_order_rejects_same_underlying() {
        assert!(component_token_order(&[1], &[1]).is_none());
    }
}
