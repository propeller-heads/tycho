use std::str::FromStr;

use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::{log, prelude::BigInt};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::factory::events::PairCreated;

use tycho_substreams::prelude::*;

#[derive(Debug, Deserialize)]
struct Params {
    factory_address: String,
    protocol_type_name: String,
    #[serde(default)]
    expose_underlying_tokens: bool,
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
    // Extract new pools from PairCreated events
    let mut on_pair_created = |event: PairCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        let Some(tokens) = resolve_component_tokens(&event, params.expose_underlying_tokens) else {
            log::info!(
                "Skipping pool {} because underlying tokens could not be resolved",
                event.pair.to_hex()
            );
            return;
        };

        let mut static_att = vec![
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
        ];

        if let Some(ring_tokens) = tokens.ring_tokens() {
            static_att.extend([
                Attribute {
                    name: "fw_token0".to_string(),
                    value: ring_tokens.fw_token0.clone(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "fw_token1".to_string(),
                    value: ring_tokens.fw_token1.clone(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "underlying_token0".to_string(),
                    value: ring_tokens.underlying_token0.clone(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "underlying_token1".to_string(),
                    value: ring_tokens.underlying_token1.clone(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "fw_decimals0".to_string(),
                    value: vec![ring_tokens.fw_decimals0],
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "fw_decimals1".to_string(),
                    value: vec![ring_tokens.fw_decimals1],
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "underlying_decimals0".to_string(),
                    value: vec![ring_tokens.underlying_decimals0],
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "underlying_decimals1".to_string(),
                    value: vec![ring_tokens.underlying_decimals1],
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "reserves_inverted".to_string(),
                    value: if ring_tokens.reserves_inverted { vec![1] } else { vec![0] },
                    change: ChangeType::Creation.into(),
                },
            ]);
        }

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
                tokens: tokens.component_tokens().to_vec(),
                contracts: vec![],
                static_att,
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
                    token: tokens.component_tokens()[0].clone(),
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event.pair.to_hex().as_bytes().to_vec(),
                },
                BalanceChange {
                    token: tokens.component_tokens()[1].clone(),
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event.pair.to_hex().as_bytes().to_vec(),
                },
            ],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![Address::from_str(&params.factory_address).unwrap()]);

    eh.on::<PairCreated, _>(&mut on_pair_created);
    eh.handle_events();
}

struct ComponentTokens {
    component_tokens: Vec<Vec<u8>>,
    ring_tokens: Option<RingTokens>,
}

impl ComponentTokens {
    fn component_tokens(&self) -> &[Vec<u8>] {
        &self.component_tokens
    }

    fn ring_tokens(&self) -> Option<&RingTokens> {
        self.ring_tokens.as_ref()
    }
}

struct RingTokens {
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

fn resolve_component_tokens(
    event: &PairCreated,
    expose_underlying_tokens: bool,
) -> Option<ComponentTokens> {
    if !expose_underlying_tokens {
        return Some(ComponentTokens {
            component_tokens: vec![event.token0.clone(), event.token1.clone()],
            ring_tokens: None,
        });
    }

    let underlying_token0 = call_underlying_token(&event.token0)?;
    let underlying_token1 = call_underlying_token(&event.token1)?;
    let fw_decimals0 = call_decimals(&event.token0)?;
    let fw_decimals1 = call_decimals(&event.token1)?;
    let underlying_decimals0 = call_decimals(&underlying_token0)?;
    let underlying_decimals1 = call_decimals(&underlying_token1)?;
    let reserves_inverted = underlying_token1 < underlying_token0;
    let component_tokens = if reserves_inverted {
        vec![underlying_token1.clone(), underlying_token0.clone()]
    } else {
        vec![underlying_token0.clone(), underlying_token1.clone()]
    };

    Some(ComponentTokens {
        component_tokens,
        ring_tokens: Some(RingTokens {
            fw_token0: event.token0.clone(),
            fw_token1: event.token1.clone(),
            underlying_token0,
            underlying_token1,
            fw_decimals0,
            fw_decimals1,
            underlying_decimals0,
            underlying_decimals1,
            reserves_inverted,
        }),
    })
}

fn call_underlying_token(wrapped_token: &[u8]) -> Option<Vec<u8>> {
    use substreams_ethereum::pb::eth::rpc;

    let rpc_calls = rpc::RpcCalls {
        calls: vec![rpc::RpcCall {
            to_addr: wrapped_token.to_vec(),
            data: ethabi::short_signature("token", &[]).to_vec(),
        }],
    };
    let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
    let response = responses.get(0)?;
    if response.failed {
        return None;
    }

    let mut values = ethabi::decode(&[ethabi::ParamType::Address], response.raw.as_ref()).ok()?;
    Some(
        values
            .pop()?
            .into_address()?
            .as_bytes()
            .to_vec(),
    )
}

fn call_decimals(token: &[u8]) -> Option<u8> {
    use substreams_ethereum::pb::eth::rpc;

    let rpc_calls = rpc::RpcCalls {
        calls: vec![rpc::RpcCall {
            to_addr: token.to_vec(),
            data: ethabi::short_signature("decimals", &[]).to_vec(),
        }],
    };
    let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
    let response = responses.get(0)?;
    if response.failed {
        return None;
    }

    let mut values = ethabi::decode(&[ethabi::ParamType::Uint(8)], response.raw.as_ref()).ok()?;
    let decimals = values.pop()?.into_uint()?.as_u32();
    u8::try_from(decimals).ok()
}
