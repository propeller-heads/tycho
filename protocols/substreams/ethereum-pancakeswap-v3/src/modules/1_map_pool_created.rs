use std::str::FromStr;

use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::factory::events::PoolCreated;

use tycho_substreams::prelude::*;

#[derive(Debug, Deserialize, PartialEq)]
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
    let params: Params = serde_qs::from_str(&params)
        .map_err(|err| anyhow::anyhow!("Invalid map_pools_created params {params:?}: {err}"))?;
    let factory_address = Address::from_str(&params.factory_address).map_err(|err| {
        anyhow::anyhow!("Invalid factory_address {:?}: {err}", params.factory_address)
    })?;

    get_new_pools(&block, &mut new_pools, factory_address, &params.protocol_type_name);

    Ok(BlockChanges { block: None, changes: new_pools })
}

// Extract new pools from PoolCreated events
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionChanges>,
    factory_address: Address,
    protocol_type_name: &str,
) {
    // Extract new pools from PoolCreated events
    let mut on_pool_created = |event: PoolCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        new_pools.push(TransactionChanges {
            tx: Some(tycho_tx.clone()),
            entity_changes: vec![EntityChanges {
                component_id: event.pool.clone().to_hex(),
                attributes: vec![
                    Attribute {
                        name: "liquidity".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "sqrt_price_x96".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.pool.to_hex(),
                tokens: vec![event.token0.clone(), event.token1.clone()],
                contracts: vec![],
                static_att: vec![
                    Attribute {
                        name: "fee".to_string(),
                        value: event.fee.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick_spacing".to_string(),
                        value: event.tick_spacing.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "pool_address".to_string(),
                        value: event.pool.clone(),
                        change: ChangeType::Creation.into(),
                    },
                ],
                change: i32::from(ChangeType::Creation),
                protocol_type: Option::from(ProtocolType {
                    name: protocol_type_name.to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
                tx: Some(tycho_tx),
            }],
            balance_changes: vec![
                BalanceChange {
                    token: event.token0,
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event
                        .pool
                        .clone()
                        .to_hex()
                        .as_bytes()
                        .to_vec(),
                },
                BalanceChange {
                    token: event.token1,
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event.pool.to_hex().as_bytes().to_vec(),
                },
            ],
            contract_changes: vec![],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![factory_address]);

    eh.on::<PoolCreated, _>(&mut on_pool_created);
    eh.handle_events();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_parse_factory_address_and_protocol_type_name() {
        let params: Params = serde_qs::from_str(
            "factory_address=ece6ecd61177336ea6fb9b17937ac439d85ee20b&protocol_type_name=gigadex_v3_pool",
        )
        .unwrap();

        assert_eq!(
            params,
            Params {
                factory_address: "ece6ecd61177336ea6fb9b17937ac439d85ee20b".to_string(),
                protocol_type_name: "gigadex_v3_pool".to_string(),
            }
        );
    }

    #[test]
    fn test_params_require_protocol_type_name() {
        assert!(serde_qs::from_str::<Params>(
            "factory_address=0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865"
        )
        .is_err());
    }
}
