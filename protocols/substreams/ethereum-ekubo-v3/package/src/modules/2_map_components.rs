use alloy_primitives::{aliases::B32, B256};
use ekubo_sdk::chain::evm::EvmPoolConfig;
use itertools::Itertools;
use substreams::scalar::BigInt;
use substreams_helper::hex::Hexable;
use tycho_substreams::models::{
    Attribute, BalanceChange, BlockChanges, ChangeType, EntityChanges, FinancialType,
    ImplementationType, ProtocolComponent, ProtocolType, TransactionChanges,
};

use crate::{
    addresses::{CORE_ADDRESS, SIGNED_EXCLUSIVE_SWAP_ADDRESS},
    pb::ekubo::{
        block_transaction_events::transaction_events::{pool_log::Event, PoolLog},
        BlockTransactionEvents,
    },
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

/// The values a component starts with: zeroes for a pool that was just initialized, the recorded
/// state for a pool restored from a seed.
struct PoolStart {
    token0: Vec<u8>,
    token1: Vec<u8>,
    config: Vec<u8>,
    tick: i32,
    sqrt_ratio: Vec<u8>,
    liquidity: Vec<u8>,
    timed: Option<TimedStart>,
}

struct TimedStart {
    rate0: Vec<u8>,
    rate1: Vec<u8>,
    last_time: u64,
}

fn maybe_create_component(
    log: PoolLog,
    timestamp: u64,
) -> Option<(ProtocolComponent, EntityChanges, Vec<BalanceChange>)> {
    let start = match log.event.unwrap() {
        Event::PoolInitialized(pi) => PoolStart {
            token0: pi.token0,
            token1: pi.token1,
            config: pi.config,
            tick: pi.tick,
            sqrt_ratio: pi.sqrt_ratio,
            liquidity: 0_u128.to_be_bytes().to_vec(),
            timed: pi
                .has_time_rate_deltas
                .then(|| TimedStart { rate0: vec![], rate1: vec![], last_time: timestamp }),
        },
        Event::PoolSnapshot(ps) => PoolStart {
            token0: ps.token0,
            token1: ps.token1,
            config: ps.config,
            tick: ps.tick,
            sqrt_ratio: ps.sqrt_ratio,
            liquidity: ps.liquidity,
            timed: ps.timed.map(|timed| TimedStart {
                rate0: timed.rate0,
                rate1: timed.rate1,
                last_time: timed.last_time,
            }),
        },
        Event::Swapped(_) |
        Event::PositionUpdated(_) |
        Event::VirtualExecution(_) |
        Event::RateUpdated(_) => return None,
    };

    let mut entity_attributes = vec![
        Attribute {
            change: ChangeType::Creation.into(),
            name: "liquidity".to_string(),
            value: start.liquidity,
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "tick".to_string(),
            value: start.tick.to_be_bytes().to_vec(),
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "sqrt_ratio".to_string(),
            value: start.sqrt_ratio,
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "balance_owner".to_string(), /* TODO: We should use AccountBalances
                                                * instead */
            value: CORE_ADDRESS.to_vec(),
        },
    ];

    if let Some(timed) = start.timed {
        entity_attributes.extend([
            Attribute {
                change: ChangeType::Creation.into(),
                name: "rate_token0".to_string(),
                value: timed.rate0,
            },
            Attribute {
                change: ChangeType::Creation.into(),
                name: "rate_token1".to_string(),
                value: timed.rate1,
            },
            Attribute {
                change: ChangeType::Creation.into(),
                name: "last_time".to_string(),
                value: timed.last_time.to_be_bytes().to_vec(),
            },
        ]);
    }

    let pool_config = EvmPoolConfig::try_from(
        B256::try_from(start.config.as_slice()).expect("pool config to be 32 bytes long"),
    )
    .expect("pool config to be valid");

    let component_id = log.pool_id.to_hex();

    let mut static_att = vec![
        Attribute {
            change: ChangeType::Creation.into(),
            name: "token0".to_string(),
            value: start.token0.clone(),
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "token1".to_string(),
            value: start.token1.clone(),
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "extension".to_string(),
            value: pool_config.extension.to_vec(),
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "fee".to_string(),
            value: pool_config.fee.to_be_bytes().to_vec(),
        },
        Attribute {
            change: ChangeType::Creation.into(),
            name: "pool_type_config".to_string(),
            value: B32::from(pool_config.pool_type_config).to_vec(),
        },
    ];

    if pool_config.extension == SIGNED_EXCLUSIVE_SWAP_ADDRESS {
        static_att.push(Attribute {
            change: ChangeType::Creation.into(),
            name: "is_exclusive".to_string(),
            value: vec![1u8],
        });
    }

    Some((
        ProtocolComponent {
            id: component_id.clone(),
            tokens: vec![start.token0.clone(), start.token1.clone()],
            contracts: vec![],
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "ekubo_v3_pool".to_string(),
                financial_type: FinancialType::Swap.into(),
                implementation_type: ImplementationType::Custom.into(),
                attribute_schema: vec![],
            }),
            static_att,
        },
        EntityChanges { component_id: component_id.clone(), attributes: entity_attributes },
        vec![
            BalanceChange {
                component_id: component_id.clone().into_bytes(),
                token: start.token0,
                balance: BigInt::zero().to_signed_bytes_be(),
            },
            BalanceChange {
                component_id: component_id.into_bytes(),
                token: start.token1,
                balance: BigInt::zero().to_signed_bytes_be(),
            },
        ],
    ))
}
