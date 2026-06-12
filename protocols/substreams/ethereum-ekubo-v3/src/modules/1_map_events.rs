use alloy_primitives::{Address, FixedBytes, Uint};
use ekubo_sdk::chain::evm::{
    float_sqrt_ratio_to_fixed, EvmOrderConfig, EvmOrderKey, EvmPoolConfig, EvmPoolKey,
};
use itertools::Itertools;
use substreams_ethereum::{
    pb::eth::{self, v2::Log},
    Event as _,
};

use crate::{
    abi::{
        boosted_fees::events as boosted_fees_events, core::events as core_events,
        twamm::events as twamm_events,
    },
    addresses::{
        BOOSTED_FEES_CONCENTRATED_ADDRESS, CORE_ADDRESS, TWAMM_ADDRESS_NEW, TWAMM_ADDRESS_OLD,
    },
    pb::ekubo::{
        block_transaction_events::{
            transaction_events::{
                pool_log::{
                    Event, PoolInitialized, PositionUpdated, RateUpdated, Swapped, VirtualExecution,
                },
                PoolLog,
            },
            TransactionEvents,
        },
        BlockTransactionEvents,
    },
};

#[substreams::handlers::map]
fn map_events(block: eth::v2::Block) -> BlockTransactionEvents {
    BlockTransactionEvents {
        block_transaction_events: block
            .transactions()
            .flat_map(|trace| {
                let pool_logs = trace
                    .logs_with_calls()
                    .filter_map(|(log, _)| maybe_pool_log(log))
                    .collect_vec();

                (!pool_logs.is_empty())
                    .then(|| TransactionEvents { transaction: Some(trace.into()), pool_logs })
            })
            .collect(),
        timestamp: block
            .header
            .as_ref()
            .unwrap()
            .timestamp
            .as_ref()
            .unwrap()
            .seconds
            .try_into()
            .unwrap(),
    }
}

fn maybe_pool_log(log: &Log) -> Option<PoolLog> {
    let emitter = Address::from_slice(&log.address);

    let (pool_id, ev) = if emitter == CORE_ADDRESS {
        if log.topics.is_empty() {
            let data = &log.data;

            assert!(data.len() == 116, "swap event data length mismatch");

            (
                data[20..52].to_vec(),
                Event::Swapped(Swapped {
                    delta0: data[52..68].to_vec(),
                    delta1: data[68..84].to_vec(),
                    sqrt_ratio_after: float_sqrt_ratio_to_fixed(Uint::from_be_slice(&data[84..96]))
                        .to_be_bytes_trimmed_vec(),
                    tick_after: i32::from_be_bytes(data[96..100].try_into().unwrap()),
                    liquidity_after: data[100..116].to_vec(),
                }),
            )
        } else if let Some(ev) = core_events::PositionUpdated::match_and_decode(log) {
            let (lower, upper) = (
                i32::from_be_bytes(
                    ev.position_id[24..28]
                        .try_into()
                        .unwrap(),
                ),
                i32::from_be_bytes(
                    ev.position_id[28..32]
                        .try_into()
                        .unwrap(),
                ),
            );

            let (delta0, delta1) =
                (ev.balance_update[0..16].to_vec(), ev.balance_update[16..32].to_vec());

            (
                ev.pool_id.to_vec(),
                Event::PositionUpdated(PositionUpdated {
                    lower,
                    upper,
                    liquidity_delta: ev.liquidity_delta.to_signed_bytes_be(),
                    delta0,
                    delta1,
                }),
            )
        } else {
            let ev = core_events::PoolInitialized::match_and_decode(log)?;
            let extension = EvmPoolConfig::try_from(FixedBytes(ev.pool_key.2))
                .expect("pool config to parse successfully")
                .extension;

            (
                ev.pool_id.to_vec(),
                Event::PoolInitialized(PoolInitialized {
                    token0: ev.pool_key.0,
                    token1: ev.pool_key.1,
                    config: ev.pool_key.2.to_vec(),
                    tick: ev.tick.to_i32(),
                    sqrt_ratio: float_sqrt_ratio_to_fixed(Uint::from_be_slice(
                        &ev.sqrt_ratio.to_bytes_be().1,
                    ))
                    .to_be_bytes_trimmed_vec(),
                    has_time_rate_deltas: [
                        TWAMM_ADDRESS_NEW,
                        TWAMM_ADDRESS_OLD,
                        BOOSTED_FEES_CONCENTRATED_ADDRESS,
                    ]
                    .contains(&extension),
                }),
            )
        }
    } else if [TWAMM_ADDRESS_NEW, TWAMM_ADDRESS_OLD].contains(&emitter) {
        if log.topics.is_empty() {
            let data = &log.data;

            assert!(data.len() == 60, "virtual execution event data length mismatch");

            (
                data[0..32].to_vec(),
                Event::VirtualExecution(VirtualExecution {
                    token0_rate: data[32..46].to_vec(),
                    token1_rate: data[46..60].to_vec(),
                }),
            )
        } else {
            let ev = twamm_events::OrderUpdated::match_and_decode(log)?;
            let order_key = EvmOrderKey {
                token0: Address::from_slice(&ev.order_key.0),
                token1: Address::from_slice(&ev.order_key.1),
                config: EvmOrderConfig::from(FixedBytes(ev.order_key.2)),
            };
            let pool_id = EvmPoolKey::from_order_key(order_key, emitter)
                .pool_id()
                .to_vec();

            let (token0_rate_delta, token1_rate_delta) = if order_key.config.is_token1 {
                (vec![], ev.sale_rate_delta.to_signed_bytes_be())
            } else {
                (ev.sale_rate_delta.to_signed_bytes_be(), vec![])
            };

            (
                pool_id,
                Event::RateUpdated(RateUpdated {
                    start_time: order_key.config.start_time,
                    end_time: order_key.config.end_time,
                    token0_rate_delta,
                    token1_rate_delta,
                }),
            )
        }
    } else if emitter == BOOSTED_FEES_CONCENTRATED_ADDRESS {
        if log.topics.is_empty() {
            let data = &log.data;

            assert!(data.len() == 60, "fees donated event data length mismatch");

            (
                data[0..32].to_vec(),
                Event::VirtualExecution(VirtualExecution {
                    token0_rate: data[32..46].to_vec(),
                    token1_rate: data[46..60].to_vec(),
                }),
            )
        } else {
            let ev = boosted_fees_events::PoolBoosted::match_and_decode(log)?;
            (
                ev.pool_id.to_vec(),
                Event::RateUpdated(RateUpdated {
                    start_time: ev.start_time.to_u64(),
                    end_time: ev.end_time.to_u64(),
                    token0_rate_delta: ev.rate0.to_signed_bytes_be(),
                    token1_rate_delta: ev.rate1.to_signed_bytes_be(),
                }),
            )
        }
    } else {
        return None;
    };

    Some(PoolLog { ordinal: log.ordinal, pool_id, event: Some(ev) })
}
