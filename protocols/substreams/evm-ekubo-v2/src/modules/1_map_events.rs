use ethabi::Address;
use itertools::Itertools;
use substreams::scalar::BigInt;
use substreams_ethereum::{
    pb::eth::{self, v2::Log},
    Event as _,
};

use crate::{
    abi::{core::events as core_events, twamm::events as twamm_events},
    deployment_config::DeploymentConfig,
    pb::ekubo::{
        block_transaction_events::{
            transaction_events::{
                pool_log::{
                    order_updated::OrderKey, pool_initialized::Extension, Event, OrderUpdated,
                    PoolInitialized, PositionUpdated, Swapped, VirtualOrdersExecuted,
                },
                PoolLog,
            },
            TransactionEvents,
        },
        BlockTransactionEvents,
    },
    pool_key::{PoolConfig, PoolKey},
    sqrt_ratio::float_sqrt_ratio_to_fixed,
};

#[substreams::handlers::map]
fn map_events(params: String, block: eth::v2::Block) -> BlockTransactionEvents {
    let config: DeploymentConfig = serde_qs::from_str(&params).unwrap();

    BlockTransactionEvents {
        block_transaction_events: block
            .transactions()
            .flat_map(|trace| {
                let pool_logs = trace
                    .logs_with_calls()
                    .filter_map(|(log, _)| maybe_pool_log(log, &config))
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

fn maybe_pool_log(log: &Log, config: &DeploymentConfig) -> Option<PoolLog> {
    let (pool_id, ev) = if log.address == config.core {
        if log.topics.is_empty() {
            let data = &log.data;

            assert!(data.len() == 116, "swap event data length mismatch");

            (
                data[20..52].to_vec(),
                Event::Swapped(Swapped {
                    delta0: data[52..68].to_vec(),
                    delta1: data[68..84].to_vec(),
                    liquidity_after: data[84..100].to_vec(),
                    sqrt_ratio_after: float_sqrt_ratio_to_fixed(BigInt::from_unsigned_bytes_be(
                        &data[100..112],
                    )),
                    tick_after: i32::from_be_bytes(data[112..116].try_into().unwrap()),
                }),
            )
        } else if let Some(ev) = core_events::PositionUpdated::match_and_decode(log) {
            (
                ev.pool_id.to_vec(),
                Event::PositionUpdated(PositionUpdated {
                    lower: ev.params.1 .0.to_i32(),
                    upper: ev.params.1 .1.to_i32(),
                    liquidity_delta: ev.params.2.to_signed_bytes_be(),
                    delta0: ev.delta0.to_signed_bytes_be(),
                    delta1: ev.delta1.to_signed_bytes_be(),
                }),
            )
        } else {
            let ev = core_events::PoolInitialized::match_and_decode(log)?;
            let pool_config = PoolConfig::from(ev.pool_key.2);

            let extension = {
                let extension = pool_config.extension.as_bytes();

                if extension == Address::zero().as_bytes() {
                    Extension::Base
                } else if extension == config.oracle {
                    Extension::Oracle
                } else if extension == config.twamm {
                    Extension::Twamm
                } else if extension == config.mev_resist {
                    Extension::MevResist
                } else {
                    Extension::Unknown
                }
            };

            (
                ev.pool_id.to_vec(),
                Event::PoolInitialized(PoolInitialized {
                    token0: ev.pool_key.0,
                    token1: ev.pool_key.1,
                    config: ev.pool_key.2.to_vec(),
                    tick: ev.tick.to_i32(),
                    sqrt_ratio: float_sqrt_ratio_to_fixed(ev.sqrt_ratio),
                    extension: extension.into(),
                }),
            )
        }
    } else if log.address == config.twamm {
        if log.topics.is_empty() {
            let data = &log.data;

            assert!(data.len() == 60, "virtual orders executed event data length mismatch");

            (
                data[0..32].to_vec(),
                Event::VirtualOrdersExecuted(VirtualOrdersExecuted {
                    token0_sale_rate: data[32..46].to_vec(),
                    token1_sale_rate: data[46..60].to_vec(),
                }),
            )
        } else {
            let ev = twamm_events::OrderUpdated::match_and_decode(log)?;
            let key = ev.order_key;

            (
                PoolKey::from_order_key(&key, &log.address).into_pool_id(),
                Event::OrderUpdated(OrderUpdated {
                    order_key: Some(OrderKey {
                        sell_token: key.0,
                        buy_token: key.1,
                        fee: key.2.to_u64(),
                        start_time: key.3.to_u64(),
                        end_time: key.4.to_u64(),
                    }),
                    sale_rate_delta: ev.sale_rate_delta.to_signed_bytes_be(),
                }),
            )
        }
    } else {
        return None;
    };

    Some(PoolLog { ordinal: log.ordinal, pool_id, event: Some(ev) })
}
