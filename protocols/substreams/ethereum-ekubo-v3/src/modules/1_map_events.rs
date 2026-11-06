use std::collections::HashMap;

use alloy_primitives::{hex, Address, FixedBytes, Uint};
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
        twamm::events as twamm_events, ve33::events as ve33_events,
    },
    addresses::{
        BOOSTED_FEES_CONCENTRATED_ADDRESS, CORE_ADDRESS, TWAMM_ADDRESS_V1, TWAMM_ADDRESS_V2,
    },
    pb::ekubo::{
        block_transaction_events::{
            transaction_events::{
                pool_log::{
                    Event, PoolInitialized, PositionUpdated, RateUpdated, SwapFeeUpdated, Swapped,
                    VirtualExecution,
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
    let launch_address = crate::params::zero_seed_launch_address(&params);

    BlockTransactionEvents {
        block_transaction_events: block
            .transactions()
            .flat_map(|trace| {
                // LaunchCreated follows Core's initialization in the same
                // transaction. Join by pool ID, not event position or token order.
                let launches: HashMap<_, _> = trace
                    .logs_with_calls()
                    .filter_map(|(log, _)| launch_config(log, launch_address))
                    .collect();
                let pool_logs = trace
                    .logs_with_calls()
                    .filter_map(|(log, _)| maybe_pool_log(log, ve33_address))
                    .map(|mut log| {
                        if let Some(Event::PoolInitialized(ref mut initialized)) = log.event {
                            if let Some(config) = launches.get(&log.pool_id) {
                                initialized.launch_config = config.clone();
                            }
                        }
                        log
                    })
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

fn maybe_pool_log(log: &Log, ve33_address: Option<Address>) -> Option<PoolLog> {
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
                    launch_config: vec![],
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

// Exact final LaunchCreated ABI: three indexed fields; the static head is
// PoolKey(3), PositionId(1), liquidity(1), LaunchParameters(11), logo offset(1).
// Metadata and the optional logo are not interpreted or fetched by the indexer.
fn launch_config(log: &Log, extension: Option<Address>) -> Option<(Vec<u8>, Vec<u8>)> {
    const TOPIC: [u8; 32] =
        hex!("0a54c7540832cb23832d30bbf27c5597cfb779d070a1c253a7aee90571c2da4c");
    if extension.is_none() ||
        extension?.as_slice() != log.address ||
        log.topics.len() != 4 ||
        log.topics[0] != TOPIC ||
        log.data.len() < 18 * 32
    {
        return None;
    }
    let data = &log.data;
    if data[16 * 32..17 * 32] != Uint::<256, 4>::from(17 * 32).to_be_bytes::<32>() {
        return None;
    }
    if data[0..12] != [0u8; 12] || data[32..44] != [0u8; 12] || data[64..84] != log.address {
        return None;
    }
    let key = EvmPoolKey {
        token0: Address::from_slice(&data[12..32]),
        token1: Address::from_slice(&data[44..64]),
        config: EvmPoolConfig::try_from(FixedBytes::from_slice(&data[64..96])).ok()?,
    };
    if key.pool_id().as_slice() != log.topics[1] {
        return None;
    }
    let logo_len = Uint::<256, 4>::from_be_slice(&data[17 * 32..18 * 32]);
    if logo_len > Uint::<256, 4>::from(data.len() - 18 * 32) {
        return None;
    }
    let mut config = Vec::with_capacity(40);
    for word in 11..15 {
        if data[word * 32..word * 32 + 24] != [0u8; 24] {
            return None;
        }
        config.extend_from_slice(&data[word * 32 + 24..(word + 1) * 32]);
    }
    config.extend_from_slice(&data[3 * 32 + 24..4 * 32]);
    Some((log.topics[1].clone(), config))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exact local Forge LaunchCreated fixture from evm-contracts
    // 1d74ed414a5a91100440b128ef07fbe15acf37a5 (empty optional logo).
    fn fixture() -> Log {
        Log {
            address: hex!("5100000000000000000000000000000000000000").to_vec(),
            topics: vec![
                hex!("0a54c7540832cb23832d30bbf27c5597cfb779d070a1c253a7aee90571c2da4c").to_vec(),
                hex!("fb5475aaf870f1b1fe76ffcb64a34cd6b1c41d22f723a49abf6bab09e5760c91").to_vec(),
                hex!("000000000000000000000000838047be0779c59e69610451ccf3fd71f5fb4a53").to_vec(),
                hex!("000000000000000000000000f62849f9a0b5bf2913b396098f7c7019b51a820a").to_vec(),
            ],
            data: hex!("0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000838047be0779c59e69610451ccf3fd71f5fb4a535100000000000000000000000000000000000000000000000000000080004646000000000000000000000000000000000000000000000000008c8c0000d28bba00000000000000000000000000000000000000000000ed8d74db520e3e09750f095a65726f205365656400000000000000000000000000000000000000000000045a45524f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000033b2e3c9fd0803ce80000000000000000000000000000000000000000000000000000000000000000000000ffffffffffffffffffffffffffffffffffffffffffffffffffffffffff2d3134ffffffffffffffffffffffffffffffffffffffffffffffffffffffffff73764400000000000000000000000000000000000000000000000000000000000003e8000000000000000000000000000000000000000000000000000000100b8a78850000000000000000000000000000000000000000000000000ccccccccccccccc00000000000000000000000000000000000000000000000000c49ba5e353f7ceabfb2c1d8ee75c05313d7d3a63c0bb9e04715890c7020bc8528b61929317e98200000000000000000000000000000000000000000000000000000000000002200000000000000000000000000000000000000000000000000000000000000000").to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn final_launch_event_and_malformed_inputs() {
        let log = fixture();
        let extension = Some(Address::from_slice(&log.address));
        let (id, config) = launch_config(&log, extension).unwrap();
        assert_eq!(id, log.topics[1]);
        let mut expected = Vec::new();
        for word in [1000u64, 68913100933, 922337203685477580, 55340232221128654] {
            expected.extend_from_slice(&word.to_be_bytes());
        }
        expected.extend_from_slice(&9210880i32.to_be_bytes());
        expected.extend_from_slice(&13798330i32.to_be_bytes());
        assert_eq!(config, expected);
        assert!(launch_config(&log, None).is_none());
        assert!(launch_config(&log, Some(Address::ZERO)).is_none());
        for len in [0, 32, 17 * 32, 18 * 32 - 1] {
            let mut truncated = log.clone();
            truncated.data.truncate(len);
            assert!(launch_config(&truncated, extension).is_none());
        }
        for offset in [16 * 32 + 31, 11 * 32, 17 * 32 + 31, 64] {
            let mut malformed = log.clone();
            malformed.data[offset] ^= 1;
            assert!(launch_config(&malformed, extension).is_none());
        }
        let mut logo = log.clone();
        logo.data[18 * 32 - 1] = 32;
        logo.data
            .extend_from_slice(b"https://example.invalid/logo.svg");
        assert_eq!(
            launch_config(&logo, extension)
                .unwrap()
                .1,
            expected
        );
    }
}
