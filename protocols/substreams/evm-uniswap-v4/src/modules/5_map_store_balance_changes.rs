use std::str::FromStr;

use anyhow::Ok;
use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas};

use crate::{
    modules::uni_math::calculate_token_amounts,
    pb::uniswap::v4::{
        events::{pool_event, PoolEvent},
        Events,
    },
};
use substreams::{
    prelude::StoreGet,
    scalar::BigInt,
    store::{StoreAddBigInt, StoreGetBigInt, StoreNew},
};

#[substreams::handlers::map]
pub fn map_balance_changes(
    events: Events,
    pools_current_sqrt_price_store: StoreGetBigInt,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let balance_deltas = events
        .pool_events
        .into_iter()
        .filter(PoolEvent::can_introduce_balance_changes)
        .map(|e| {
            (
                pools_current_sqrt_price_store
                    .get_at(e.log_ordinal, format!("pool:{0}", &e.pool_id))
                    .unwrap_or(BigInt::zero()),
                e,
            )
        })
        .filter_map(|(current_sqrt_price, event)| {
            event_to_balance_deltas(current_sqrt_price, event)
        })
        .flatten()
        .collect();

    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}

fn event_to_balance_deltas(
    current_sqrt_price: BigInt,
    event: PoolEvent,
) -> Option<Vec<BalanceDelta>> {
    let address = event.pool_id.as_bytes().to_vec();
    match event.r#type.unwrap() {
        pool_event::Type::ModifyLiquidity(e) => {
            let (delta0, delta1) =
                get_amount_delta(current_sqrt_price, e.tick_lower, e.tick_upper, e.liquidity_delta);
            Some(vec![
                BalanceDelta {
                    token: hex::decode(event.currency0.trim_start_matches("0x")).unwrap(),
                    delta: delta0.to_signed_bytes_be(),
                    component_id: address.clone(),
                    ord: event.log_ordinal,
                    tx: event
                        .transaction
                        .as_ref()
                        .map(Into::into),
                },
                BalanceDelta {
                    token: hex::decode(event.currency1.trim_start_matches("0x")).unwrap(),
                    delta: delta1.to_signed_bytes_be(),
                    component_id: address,
                    ord: event.log_ordinal,
                    tx: event
                        .transaction
                        .as_ref()
                        .map(Into::into),
                },
            ])
        }

        pool_event::Type::Swap(e) => {
            let mut delta0 = BigInt::from_str(&e.amount0)
                .unwrap()
                .neg();
            let mut delta1 = BigInt::from_str(&e.amount1)
                .unwrap()
                .neg();

            // Adjusting deltas to exclude collected fees.
            // Collected fees are not considered part of the component balance since they aren't
            // accounted for during swaps. Thus, they should be excluded from the Total Value Locked
            // (TVL).
            // To achieve this, we subtract the fee portion from each delta.

            // We perform integer division and round to the nearest integer based on the
            // remainder. If the remainder is at least half of the divisor, we
            // round up.
            let bips_divisor = BigInt::from(1_000_000);
            let half_divisor = BigInt::from(500_000);
            if delta0 > BigInt::zero() {
                let fee_part = delta0.clone() * e.fee;
                let (quotient, remainder) = fee_part.div_rem(&bips_divisor);
                delta0 =
                    delta0 - if remainder >= half_divisor { quotient + 1u32 } else { quotient };
            }
            if delta1 > BigInt::zero() {
                let fee_part = delta1.clone() * e.fee;
                let (quotient, remainder) = fee_part.div_rem(&bips_divisor);
                delta1 =
                    delta1 - if remainder >= half_divisor { quotient + 1u32 } else { quotient };
            }

            Some(vec![
                BalanceDelta {
                    token: hex::decode(event.currency0.trim_start_matches("0x")).unwrap(),
                    delta: delta0.to_signed_bytes_be(),
                    component_id: address.clone(),
                    ord: event.log_ordinal,
                    tx: event
                        .transaction
                        .as_ref()
                        .map(Into::into),
                },
                BalanceDelta {
                    token: hex::decode(event.currency1.trim_start_matches("0x")).unwrap(),
                    delta: delta1.to_signed_bytes_be(),
                    component_id: address,
                    ord: event.log_ordinal,
                    tx: event
                        .transaction
                        .as_ref()
                        .map(Into::into),
                },
            ])
        }
        _ => None,
    }
}

impl PoolEvent {
    fn can_introduce_balance_changes(&self) -> bool {
        matches!(
            self.r#type.as_ref().unwrap(),
            pool_event::Type::ModifyLiquidity(_) | pool_event::Type::Swap(_)
        )
    }
}

fn get_amount_delta(
    current_sqrt_price: BigInt,
    tick_lower: i32,
    tick_upper: i32,
    liquidity_delta: String,
) -> (BigInt, BigInt) {
    // This should never fail because the liquidity delta is a string encoded signed int128 (from
    // the contract)
    let liquidity_delta: i128 = liquidity_delta
        .parse()
        .expect("Failed to parse liquidity delta");

    let (amount0, amount1) =
        calculate_token_amounts(current_sqrt_price.into(), tick_lower, tick_upper, liquidity_delta)
            .expect("Failed to calculate token amounts from liquidity delta");
    (BigInt::from(amount0), BigInt::from(amount1))
}
