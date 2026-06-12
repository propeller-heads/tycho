use std::str::FromStr;

use num_bigint::BigInt;
use num_traits::Zero as _;

use crate::{
    events::{PoolEvent, PoolEventKind},
    math::calculate_token_amounts,
};

pub struct BalanceDelta {
    pub token: Vec<u8>,
    pub component_id: Vec<u8>,
    pub delta: BigInt,
}

/// Computes the component balance deltas an event produces.
///
/// `current_sqrt_price` must be the pool's sqrt price as of the event (set by the
/// last `Initialize` or `Swap` before it) — `ModifyLiquidity` amounts depend on it.
///
/// Swap deltas exclude the LP fee portion: collected fees are not part of the
/// component balance since they aren't accounted for during swaps, so they are
/// subtracted (rounded half-up) from the incoming-token delta.
pub fn event_to_balance_deltas(
    current_sqrt_price: &BigInt,
    event: &PoolEvent,
) -> Vec<BalanceDelta> {
    let component_id = event.pool_id.clone();

    match &event.kind {
        PoolEventKind::ModifyLiquidity { tick_lower, tick_upper, liquidity_delta } => {
            let Ok(liquidity_delta) = liquidity_delta.parse::<i128>() else {
                return vec![];
            };
            let Ok((delta0, delta1)) = calculate_token_amounts(
                current_sqrt_price.clone(),
                *tick_lower,
                *tick_upper,
                liquidity_delta,
            ) else {
                return vec![];
            };
            vec![
                BalanceDelta {
                    token: event.currency0.clone(),
                    component_id: component_id.clone(),
                    delta: delta0,
                },
                BalanceDelta { token: event.currency1.clone(), component_id, delta: delta1 },
            ]
        }
        PoolEventKind::Swap { amount0, amount1, fee, .. } => {
            let delta0 = -BigInt::from_str(amount0).unwrap_or_default();
            let delta1 = -BigInt::from_str(amount1).unwrap_or_default();
            vec![
                BalanceDelta {
                    token: event.currency0.clone(),
                    component_id: component_id.clone(),
                    delta: subtract_fee(delta0, *fee),
                },
                BalanceDelta {
                    token: event.currency1.clone(),
                    component_id,
                    delta: subtract_fee(delta1, *fee),
                },
            ]
        }
        PoolEventKind::Initialize { .. } | PoolEventKind::ProtocolFeeUpdated { .. } => vec![],
    }
}

/// Removes the LP fee portion from a positive (incoming) swap delta.
///
/// Fees are expressed in hundredths of a bip (1e-6). Integer division rounds to
/// the nearest integer: remainders of at least half the divisor round up.
fn subtract_fee(delta: BigInt, fee: u32) -> BigInt {
    if delta <= BigInt::zero() {
        return delta;
    }
    let bips_divisor = BigInt::from(1_000_000u32);
    let half_divisor = BigInt::from(500_000u32);
    let fee_part = &delta * fee;
    let quotient = &fee_part / &bips_divisor;
    let remainder = &fee_part % &bips_divisor;
    delta - if remainder >= half_divisor { quotient + 1u32 } else { quotient }
}
