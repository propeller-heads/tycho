use std::str::FromStr;

use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::prelude::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::factory::events::PoolCreated;

use tycho_substreams::prelude::*;

#[derive(Debug, Deserialize)]
struct Params {
    factory_address: String,
}

fn bool_to_bytes(value: bool) -> Vec<u8> {
    vec![u8::from(value)]
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
    let mut on_pool_created = |event: PoolCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        new_pools.push(TransactionChanges {
            tx: Some(tycho_tx.clone()),
            contract_changes: vec![],
            entity_changes: vec![EntityChanges {
                component_id: event.pool.to_hex(),
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
                    Attribute {
                        name: "fee".to_string(),
                        // Pool creation starts without a custom fee override.
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
                        name: "is_stable".to_string(),
                        value: bool_to_bytes(event.stable),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "pool_address".to_string(),
                        value: event.pool.clone(),
                        change: ChangeType::Creation.into(),
                    },
                ],
                change: i32::from(ChangeType::Creation),
                protocol_type: Some(ProtocolType {
                    name: "aerodrome_v1_pool".to_string(),
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
                    component_id: event.pool.to_hex().as_bytes().to_vec(),
                },
                BalanceChange {
                    token: event.token1,
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event.pool.to_hex().as_bytes().to_vec(),
                },
            ],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![Address::from_str(&params.factory_address).unwrap()]);

    eh.on::<PoolCreated, _>(&mut on_pool_created);
    eh.handle_events();
}
