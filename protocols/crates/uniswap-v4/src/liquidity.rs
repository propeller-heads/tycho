use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub enum LiquidityChangeKind {
    Delta,
    Absolute,
}

pub struct LiquidityDelta {
    pub pool_id: Vec<u8>,
    pub value: BigInt,
    pub kind: LiquidityChangeKind,
}

/// Computes the change to a pool's active liquidity for an event.
///
/// `ModifyLiquidity` contributes a delta only when the position straddles the
/// current tick; `Swap` carries the pool's absolute post-swap liquidity.
pub fn event_to_liquidity_delta(current_tick: i64, event: &PoolEvent) -> Option<LiquidityDelta> {
    match &event.kind {
        PoolEventKind::ModifyLiquidity { tick_lower, tick_upper, liquidity_delta } => {
            if current_tick >= i64::from(*tick_lower) && current_tick < i64::from(*tick_upper) {
                Some(LiquidityDelta {
                    pool_id: event.pool_id.clone(),
                    value: BigInt::from_str(liquidity_delta).unwrap_or_default(),
                    kind: LiquidityChangeKind::Delta,
                })
            } else {
                None
            }
        }
        PoolEventKind::Swap { liquidity, .. } => Some(LiquidityDelta {
            pool_id: event.pool_id.clone(),
            value: BigInt::from_str(liquidity).unwrap_or_default(),
            kind: LiquidityChangeKind::Absolute,
        }),
        PoolEventKind::Initialize { .. } | PoolEventKind::ProtocolFeeUpdated { .. } => None,
    }
}

pub fn event_to_current_tick(event: &PoolEvent) -> Option<i64> {
    match &event.kind {
        PoolEventKind::Initialize { tick, .. } => Some(i64::from(*tick)),
        PoolEventKind::Swap { tick, .. } => Some(i64::from(*tick)),
        PoolEventKind::ModifyLiquidity { .. } | PoolEventKind::ProtocolFeeUpdated { .. } => None,
    }
}

pub fn event_to_current_sqrt_price(event: &PoolEvent) -> Option<BigInt> {
    match &event.kind {
        PoolEventKind::Initialize { sqrt_price, .. } | PoolEventKind::Swap { sqrt_price, .. } => {
            BigInt::from_str(sqrt_price).ok()
        }
        PoolEventKind::ModifyLiquidity { .. } | PoolEventKind::ProtocolFeeUpdated { .. } => None,
    }
}
