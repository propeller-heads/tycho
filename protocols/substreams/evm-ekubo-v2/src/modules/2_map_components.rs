use itertools::Itertools;
use substreams::{hex, scalar::BigInt};
use substreams_helper::hex::Hexable;
use tycho_substreams::models::{
    Attribute, BalanceChange, BlockChanges, ChangeType, EntityChanges, FinancialType,
    ImplementationType, ProtocolComponent, ProtocolType, TransactionChanges,
};

use crate::{
    pb::ekubo::{
        block_transaction_events::transaction_events::{
            pool_log::{pool_initialized::Extension, Event},
            PoolLog,
        },
        BlockTransactionEvents,
    },
    pool_key::PoolConfig,
};

#[substreams::handlers::map]
fn map_components(block_tx_events: BlockTransactionEvents) -> BlockChanges {
    BlockChanges {
        block: None,
        changes: block_tx_events
            .block_transaction_events
            .into_iter()
            .filter_map(|tx_events| {
                let (components, entities, balance_changes): (Vec<_>, Vec<_>, Vec<_>) = tx_events
                    .pool_logs
                    .into_iter()
                    .filter_map(|log| maybe_create_component(log, block_tx_events.timestamp))
                    .multiunzip();

                (!components.is_empty()).then(|| TransactionChanges {
                    tx: Some(tx_events.transaction.unwrap().into()),
                    balance_changes: balance_changes
                        .into_iter()
                        .flatten()
                        .collect(),
                    contract_changes: vec![],
                    entity_changes: entities,
                    component_changes: components,
                    ..Default::default()
                })
            })
            .collect(),
        ..Default::default()
    }
}

fn maybe_create_component(
    log: PoolLog,
    timestamp: u64,
) -> Option<(ProtocolComponent, EntityChanges, Vec<BalanceChange>)> {
    if let Event::PoolInitialized(pi) = log.event.unwrap() {
        let entity_attributes = (pi.extension() == Extension::Twamm)
            .then(|| {
                [
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "token0_sale_rate".to_string(),
                        value: vec![],
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "token1_sale_rate".to_string(),
                        value: vec![],
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "last_execution_time".to_string(),
                        value: timestamp.to_be_bytes().to_vec(),
                    },
                ]
            })
            .into_iter()
            .flatten()
            .chain([
                Attribute {
                    change: ChangeType::Creation.into(),
                    name: "liquidity".to_string(),
                    value: 0_u128.to_be_bytes().to_vec(),
                },
                Attribute {
                    change: ChangeType::Creation.into(),
                    name: "tick".to_string(),
                    value: pi.tick.to_be_bytes().to_vec(),
                },
                Attribute {
                    change: ChangeType::Creation.into(),
                    name: "sqrt_ratio".to_string(),
                    value: pi.sqrt_ratio,
                },
                Attribute {
                    change: ChangeType::Creation.into(),
                    name: "balance_owner".to_string(), /* TODO: We should use AccountBalances
                                                        * instead */
                    value: hex!("e0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444").to_vec(),
                },
            ])
            .collect();

        let config = PoolConfig::from(<[u8; 32]>::try_from(pi.config).unwrap());
        let component_id = log.pool_id.to_hex();

        return Some((
            ProtocolComponent {
                id: component_id.clone(),
                tokens: vec![pi.token0.clone(), pi.token1.clone()],
                contracts: vec![],
                change: ChangeType::Creation.into(),
                protocol_type: Some(ProtocolType {
                    name: "ekubo_v2_pool".to_string(),
                    financial_type: FinancialType::Swap.into(),
                    implementation_type: ImplementationType::Custom.into(),
                    attribute_schema: vec![],
                }),
                // Order of attributes matters (used in store_pool_details)
                static_att: vec![
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "token0".to_string(),
                        value: pi.token0.clone(),
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "token1".to_string(),
                        value: pi.token1.clone(),
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "fee".to_string(),
                        value: config.fee.to_be_bytes().to_vec(),
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "tick_spacing".to_string(),
                        value: config
                            .tick_spacing
                            .to_be_bytes()
                            .to_vec(),
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "extension".to_string(),
                        value: config
                            .extension
                            .to_fixed_bytes()
                            .to_vec(),
                    },
                    Attribute {
                        change: ChangeType::Creation.into(),
                        name: "extension_id".to_string(),
                        value: pi.extension.to_be_bytes().to_vec(),
                    },
                ],
            },
            EntityChanges { component_id: component_id.clone(), attributes: entity_attributes },
            vec![
                BalanceChange {
                    component_id: component_id.clone().into_bytes(),
                    token: pi.token0,
                    balance: BigInt::zero().to_signed_bytes_be(),
                },
                BalanceChange {
                    component_id: component_id.into_bytes(),
                    token: pi.token1,
                    balance: BigInt::zero().to_signed_bytes_be(),
                },
            ],
        ));
    }

    None
}
