use alloy_primitives::{Address, FixedBytes, Uint};
use ekubo_sdk::chain::evm::{
    float_sqrt_ratio_to_fixed, EvmOrderConfig, EvmOrderKey, EvmPoolConfig, EvmPoolKey,
};
use itertools::Itertools;
use substreams_ethereum::{
    pb::eth::{
        self,
        v2::{Log, TransactionTrace},
    },
    Event as _,
};

use crate::{
    abi::{
        boosted_fees::events as boosted_fees_events, core::events as core_events,
        twamm::events as twamm_events, ve33::events as ve33_events,
    },
    addresses::{
        BOOSTED_FEES_CONCENTRATED_ADDRESS, CORE_ADDRESS, TWAMM_ADDRESS_V1, TWAMM_ADDRESS_V2,
    },
    pb::ekubo::{
        block_transaction_events::{
            transaction_events::{
                pool_log::{
                    Event, ExtensionStateUpdated, PoolInitialized, PositionUpdated, RateUpdated,
                    SwapFeeUpdated, Swapped, VirtualExecution,
                },
                PoolLog,
            },
            TransactionEvents,
        },
        BlockTransactionEvents,
    },
};

#[substreams::handlers::map]
fn map_events(params: String, block: eth::v2::Block) -> BlockTransactionEvents {
    let ve33_address = crate::params::ve33_address(&params);
    let mev_capture_v21_address = crate::params::mev_capture_v21_address(&params);

    BlockTransactionEvents {
        block_transaction_events: block
            .transactions()
            .flat_map(|trace| {
                let pool_logs = trace
                    .logs_with_calls()
                    .filter_map(|(log, _)| {
                        maybe_pool_log(log, ve33_address, mev_capture_v21_address)
                    })
                    .chain(
                        mev_capture_v21_address
                            .map(|extension| extension_state_updates(trace, extension))
                            .unwrap_or_default(),
                    )
                    .sorted_unstable_by_key(|log| log.ordinal)
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

/// The MEVCapture v2.1 extension keeps its per-pool state in the storage slot
/// keyed by the pool ID and emits no event when it changes, so track the last
/// value written in the transaction to each slot.
fn extension_state_updates(trace: &TransactionTrace, extension: Address) -> Vec<PoolLog> {
    trace
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter(|change| change.address == extension.as_slice())
        .into_group_map_by(|change| change.key.clone())
        .into_iter()
        .map(|(pool_id, changes)| {
            let last = changes
                .into_iter()
                .max_by_key(|change| change.ordinal)
                .unwrap();

            PoolLog {
                ordinal: last.ordinal,
                pool_id,
                event: Some(Event::ExtensionStateUpdated(ExtensionStateUpdated {
                    state: last.new_value.clone(),
                })),
            }
        })
        .collect()
}

fn maybe_pool_log(
    log: &Log,
    ve33_address: Option<Address>,
    mev_capture_v21_address: Option<Address>,
) -> Option<PoolLog> {
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
                        TWAMM_ADDRESS_V2,
                        TWAMM_ADDRESS_V1,
                        BOOSTED_FEES_CONCENTRATED_ADDRESS,
                    ]
                    .contains(&extension),
                    is_mev_capture_v21: Some(extension) == mev_capture_v21_address,
                }),
            )
        }
    } else if [TWAMM_ADDRESS_V2, TWAMM_ADDRESS_V1].contains(&emitter) {
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
    } else if Some(emitter) == ve33_address {
        let ev = ve33_events::VoteWeightApplied::match_and_decode(log)?;
        (
            ev.pool_id.to_vec(),
            Event::SwapFeeUpdated(SwapFeeUpdated {
                swap_fee: ev
                    .swap_fee
                    .to_u64()
                    .to_be_bytes()
                    .to_vec(),
            }),
        )
    } else {
        return None;
    };

    Some(PoolLog { ordinal: log.ordinal, pool_id, event: Some(ev) })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use substreams_ethereum::pb::eth::v2::{Call, StorageChange};

    use super::*;

    const EXTENSION: Address = address!("0x51f831c23fa4051862881Ef8EfC0c3C37905Ce49");

    fn change(address: Address, key: u8, value: u8, ordinal: u64) -> StorageChange {
        StorageChange {
            address: address.to_vec(),
            key: vec![key; 32],
            old_value: vec![0; 32],
            new_value: vec![value; 32],
            ordinal,
        }
    }

    #[test]
    fn test_extension_state_updates_take_the_last_write_per_pool() {
        let trace = TransactionTrace {
            calls: vec![
                Call {
                    storage_changes: vec![
                        change(EXTENSION, 1, 10, 5),
                        change(EXTENSION, 2, 20, 6),
                        change(CORE_ADDRESS, 1, 99, 7),
                    ],
                    ..Default::default()
                },
                Call {
                    state_reverted: true,
                    storage_changes: vec![change(EXTENSION, 1, 66, 9)],
                    ..Default::default()
                },
                Call { storage_changes: vec![change(EXTENSION, 1, 11, 8)], ..Default::default() },
            ],
            ..Default::default()
        };

        let updates: Vec<_> = extension_state_updates(&trace, EXTENSION)
            .into_iter()
            .sorted_unstable_by_key(|log| log.ordinal)
            .map(|log| {
                let Some(Event::ExtensionStateUpdated(ev)) = log.event else {
                    panic!("unexpected event");
                };
                (log.pool_id[0], ev.state[0], log.ordinal)
            })
            .collect();

        assert_eq!(updates, vec![(2, 20, 6), (1, 11, 8)]);
    }
}
