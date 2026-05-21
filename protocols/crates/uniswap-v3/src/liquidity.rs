use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub enum LiquidityChangeKind {
    Delta,
    Absolute,
}

pub struct LiquidityDelta {
    pub pool_address: Vec<u8>,
    pub value: BigInt,
    pub kind: LiquidityChangeKind,
}

pub fn event_to_liquidity_delta(current_tick: i64, event: &PoolEvent) -> Option<LiquidityDelta> {
    match &event.kind {
        PoolEventKind::Mint { tick_lower, tick_upper, amount, .. } => {
            if current_tick >= i64::from(*tick_lower) && current_tick < i64::from(*tick_upper) {
                Some(LiquidityDelta {
                    pool_address: event.pool_address.clone(),
                    value: BigInt::from_str(amount).unwrap_or_default(),
                    kind: LiquidityChangeKind::Delta,
                })
            } else {
                None
            }
        }
        PoolEventKind::Burn { tick_lower, tick_upper, amount, .. } => {
            if current_tick >= i64::from(*tick_lower) && current_tick < i64::from(*tick_upper) {
                Some(LiquidityDelta {
                    pool_address: event.pool_address.clone(),
                    value: -BigInt::from_str(amount).unwrap_or_default(),
                    kind: LiquidityChangeKind::Delta,
                })
            } else {
                None
            }
        }
        PoolEventKind::Swap { liquidity, .. } => Some(LiquidityDelta {
            pool_address: event.pool_address.clone(),
            value: BigInt::from_str(liquidity).unwrap_or_default(),
            kind: LiquidityChangeKind::Absolute,
        }),
        _ => None,
    }
}

pub fn event_to_current_tick(event: &PoolEvent) -> Option<i64> {
    match &event.kind {
        PoolEventKind::Initialize { tick, .. } => Some(i64::from(*tick)),
        PoolEventKind::Swap { tick, .. } => Some(i64::from(*tick)),
        _ => None,
    }
}
