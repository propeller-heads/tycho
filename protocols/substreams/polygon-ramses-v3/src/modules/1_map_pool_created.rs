use std::str::FromStr;

use ethabi::ethereum_types::Address;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::factory::events::PoolCreated;

use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pools_created(params: String, block: eth::Block) -> BlockEntityChanges {
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];
    let factory_address = Address::from_str(&params).expect("Invalid factory address");

    get_new_pools(&block, &mut new_pools, factory_address);

    BlockEntityChanges { block: None, changes: new_pools }
}

// Extract new pools from PoolCreated events.
//
// Unlike Uniswap V3, the Ramses pool fee is governance-mutable (the pool emits a `FeeAdjustment`
// event, handled in `map_events`), so `fee` is emitted as an updatable entity attribute rather than
// a static one. `tick_spacing` is the immutable key of the pool and stays static.
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    factory_address: Address,
) {
    let mut on_pool_created = |event: PoolCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        new_pools.push(TransactionEntityChanges {
            tx: Some(tycho_tx.clone()),
            entity_changes: vec![EntityChanges {
                component_id: event.pool.to_hex(),
                attributes: vec![
                    Attribute {
                        name: "liquidity".to_string(),
                        value: BigInt::from(0).to_bytes_be().1,
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "sqrt_price_x96".to_string(),
                        value: BigInt::from(0).to_bytes_be().1,
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "fee".to_string(),
                        value: event.fee.to_bytes_be().1,
                        change: ChangeType::Creation.into(),
                    },
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.pool.to_hex(),
                tokens: vec![event.token0, event.token1],
                contracts: vec![],
                static_att: vec![Attribute {
                    name: "tick_spacing".to_string(),
                    value: event.tick_spacing.to_bytes_be().1,
                    change: ChangeType::Creation.into(),
                }],
                change: ChangeType::Creation.into(),
                protocol_type: Some(ProtocolType {
                    name: "ramses_v3_pool".to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
            }],
            balance_changes: vec![],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![factory_address]);

    eh.on::<PoolCreated, _>(&mut on_pool_created);
    eh.handle_events();
}
