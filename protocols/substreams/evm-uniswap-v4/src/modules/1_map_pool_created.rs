use std::str::FromStr;

use ethabi::ethereum_types::Address;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::pool_manager::events::Initialize;

use tycho_substreams::prelude::*;
#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];
    let pool_manager = params.as_str();

    get_new_pools(&block, &mut new_pools, pool_manager);

    Ok(BlockEntityChanges { block: None, changes: new_pools })
}

// Extract new pools initialized on the pool manager contract
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    pool_manager_address: &str,
) {
    // Extract new pools from Initialize events
    let mut on_pool_created = |event: Initialize, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();

        new_pools.push(TransactionEntityChanges {
            tx: Some(tycho_tx.clone()),
            entity_changes: vec![EntityChanges {
                component_id: event.id.to_vec().to_hex(),
                attributes: vec![
                    Attribute {
                        name: "balance_owner".to_string(),
                        value: hex::decode(pool_manager_address).unwrap(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "liquidity".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick".to_string(),
                        value: event.tick.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "sqrt_price_x96".to_string(),
                        value: event
                            .sqrt_price_x96
                            .to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "protocol_fees/zero2one".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "protocol_fees/one2zero".to_string(),
                        value: BigInt::from(0).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.id.to_vec().to_hex(),
                tokens: vec![event.currency0.clone(), event.currency1.clone()],
                contracts: vec![],
                static_att: vec![
                    Attribute {
                        name: "tick_spacing".to_string(),
                        value: event.tick_spacing.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "pool_id".to_string(),
                        value: event.id.to_vec(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "hooks".to_string(),
                        value: event.hooks.to_vec(),
                        change: ChangeType::Creation.into(),
                    },
                    // Represents the pool's LP Fee. The fee is either static or dynamic. Static
                    // fees are represented in hundredths of a bip, can be set to a value between 0
                    // and 1000000 (100%) and are immutable. If the value is set to 0x800000 then
                    // the pool is flagged as using dynamic fees. The dynamic fees changes should
                    // be tracked in state attributes and the flag value set
                    // here should remain untouched (it is needed for
                    // generating the PoolKey for contract interactions).
                    Attribute {
                        name: "key_lp_fee".to_string(),
                        value: event.fee.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                ],
                change: i32::from(ChangeType::Creation),
                protocol_type: Some(ProtocolType {
                    name: "uniswap_v4_pool".to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
            }],
            balance_changes: vec![
                BalanceChange {
                    token: event.currency0,
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event
                        .id
                        .to_vec()
                        .to_hex()
                        .as_bytes()
                        .to_vec(),
                },
                BalanceChange {
                    token: event.currency1,
                    balance: BigInt::from(0).to_signed_bytes_be(),
                    component_id: event
                        .id
                        .to_vec()
                        .to_hex()
                        .as_bytes()
                        .to_vec(),
                },
            ],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![Address::from_str(pool_manager_address).unwrap()]);

    eh.on::<Initialize, _>(&mut on_pool_created);
    eh.handle_events();
}
