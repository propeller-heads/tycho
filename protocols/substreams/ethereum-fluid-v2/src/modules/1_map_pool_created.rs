use std::str::FromStr;

use ethabi::ethereum_types::Address;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::event_handler::EventHandler;

use crate::{
    abi::d3_user_module::events::LogInitialize,
    modules::utils::{self, Params},
};

use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionChanges> = vec![];
    let params = Params::parse_from_query(&params)?;
    let dex_v2_address = Address::from_str(&params.dex_v2_address).expect("Invalid dex_v2_address");
    get_new_pools(&block, &mut new_pools, dex_v2_address);
    Ok(BlockChanges { block: Some((&block).into()), changes: new_pools, ..Default::default() })
}

fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionChanges>,
    dex_v2_address: Address,
) {
    let mut on_pool_initialize =
        |event: LogInitialize, _tx: &eth::TransactionTrace, _log: &eth::Log| {
            let component_id = utils::component_id(&event.dex_type, &event.dex_id);
            new_pools.push(TransactionChanges {
                tx: Some(_tx.into()),
                contract_changes: vec![],
                entity_changes: vec![EntityChanges {
                    component_id: component_id.clone(),
                    attributes: vec![
                        Attribute {
                            name: "dex_variables".to_string(),
                            value: vec![0u8; 32],
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "dex_variables2".to_string(),
                            value: vec![0u8; 32],
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "token0/token_reserves".to_string(),
                            value: vec![0u8; 16],
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "token1/token_reserves".to_string(),
                            value: vec![0u8; 16],
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "token0/borrow_exchange_price".to_string(),
                            value: BigInt::from(0).to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "token0/supply_exchange_price".to_string(),
                            value: BigInt::from(0).to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "token1/borrow_exchange_price".to_string(),
                            value: BigInt::from(0).to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "token1/supply_exchange_price".to_string(),
                            value: BigInt::from(0).to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                    ],
                }],
                component_changes: vec![ProtocolComponent {
                    id: component_id,
                    tokens: vec![event.dex_key.0, event.dex_key.1],
                    contracts: vec![],
                    static_att: vec![
                        Attribute {
                            name: "dex_type".to_string(),
                            value: event.dex_type.to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "fee".to_string(),
                            value: event.dex_key.2.to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "tick_spacing".to_string(),
                            value: event.dex_key.3.to_signed_bytes_be(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "controller".to_string(),
                            value: event.dex_key.4,
                            change: ChangeType::Creation.into(),
                        },
                    ],
                    change: i32::from(ChangeType::Creation),
                    protocol_type: Option::from(ProtocolType {
                        name: "fluid_v2_pool".to_string(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: ImplementationType::Custom.into(),
                    }),
                }],
                balance_changes: vec![],
                ..Default::default()
            });
        };
    let mut eh = EventHandler::new(block);

    eh.filter_by_address(dex_v2_address);

    eh.on::<LogInitialize, _>(&mut on_pool_initialize);
    eh.handle_events();
}
