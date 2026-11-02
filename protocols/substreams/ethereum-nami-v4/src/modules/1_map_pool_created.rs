use std::str::FromStr;

use ethabi::ethereum_types::Address;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::factory::events::Pool;

use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pools_created(params: String, block: eth::Block) -> BlockEntityChanges {
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];
    let factory_address = Address::from_str(&params).expect("Invalid factory address");

    get_new_pools(&block, &mut new_pools, factory_address);

    BlockEntityChanges { block: None, changes: new_pools }
}

// Extract new pools from the factory's `Pool` event, which Nami also emits for custom pools.
//
// An Algebra pool has no fee tier: its fee, tick spacing and plugin are set by pool events
// (`Fee`, `Plugin`, `PluginConfig`) emitted in the creation and initialization transactions, so
// every attribute starts at zero here and is filled in by `map_events`.
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    factory_address: Address,
) {
    let mut on_pool_created = |event: Pool, tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = tx.into();
        let zero_attribute = |name: &str, value: Vec<u8>| Attribute {
            name: name.to_string(),
            value,
            change: ChangeType::Creation.into(),
        };

        new_pools.push(TransactionEntityChanges {
            tx: Some(tycho_tx),
            entity_changes: vec![EntityChanges {
                component_id: event.pool.to_hex(),
                attributes: vec![
                    zero_attribute("liquidity", BigInt::from(0).to_bytes_be().1),
                    zero_attribute("tick", BigInt::from(0).to_signed_bytes_be()),
                    zero_attribute("sqrt_price_x96", BigInt::from(0).to_bytes_be().1),
                    zero_attribute("fee", BigInt::from(0).to_bytes_be().1),
                    zero_attribute("plugin_config", BigInt::from(0).to_bytes_be().1),
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.pool.to_hex(),
                tokens: vec![event.token0, event.token1],
                contracts: vec![],
                static_att: vec![],
                change: ChangeType::Creation.into(),
                protocol_type: Some(ProtocolType {
                    name: "nami_v4_pool".to_string(),
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

    eh.on::<Pool, _>(&mut on_pool_created);
    eh.handle_events();
}
