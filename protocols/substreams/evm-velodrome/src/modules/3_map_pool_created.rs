use crate::{abi::factory::events::PoolCreated, modules::utils::Params};
use ethabi::ethereum_types::Address;
use std::str::FromStr;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetInt64},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};
use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
    tick_spacing_to_fee_store: StoreGetInt64,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionChanges> = vec![];
    let params = Params::parse_from_query(&params)?;
    let factory_addresses = params
        .factories
        .iter()
        .map(|f| Address::from_str(f).expect("invalid address"))
        .collect::<Vec<_>>();
    get_new_pools(&block, &mut new_pools, factory_addresses, tick_spacing_to_fee_store);

    Ok(BlockChanges { block: Some((&block).into()), changes: new_pools, ..Default::default() })
}

// Extract new pools from PoolCreated events
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionChanges>,
    factory_addresses: Vec<Address>,
    tick_spacing_to_fee_store: StoreGetInt64,
) {
    // Extract new pools from PoolCreated events
    let mut on_pool_created = |event: PoolCreated, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();
        // Get default fee for tick spacing
        let default_fee = tick_spacing_to_fee_store
            .get_last(format!("tick_spacing_{}", event.tick_spacing))
            .expect("Failed to get default fee");
        new_pools.push(TransactionChanges {
            tx: Some(tycho_tx.clone()),
            contract_changes: vec![],
            entity_changes: vec![EntityChanges {
                component_id: event.pool.to_hex(),
                attributes: vec![
                    Attribute {
                        name: "liquidity".to_string(),
                        value: 0_u128.to_be_bytes().to_vec(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick".to_string(),
                        value: 0_i32.to_be_bytes().to_vec(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "sqrt_price_x96".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "custom_fee".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.pool.to_hex(),
                tokens: vec![event.token0, event.token1],
                contracts: vec![],
                static_att: vec![
                    Attribute {
                        name: "default_fee".to_string(),
                        value: BigInt::from(default_fee).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick_spacing".to_string(),
                        value: event.tick_spacing.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "pool_address".to_string(),
                        value: event.pool,
                        change: ChangeType::Creation.into(),
                    },
                ],
                change: i32::from(ChangeType::Creation),
                protocol_type: Option::from(ProtocolType {
                    name: "velodrome_slipstreams_pool".to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
            }],
            balance_changes: vec![],
            ..Default::default()
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(factory_addresses);

    eh.on::<PoolCreated, _>(&mut on_pool_created);
    eh.handle_events();
}
