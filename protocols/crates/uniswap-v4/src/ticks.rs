use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct TickDelta {
    pub pool_id: Vec<u8>,
    pub tick_index: i32,
    pub liquidity_net_delta: BigInt,
}

/// Computes per-tick net-liquidity deltas for an event.
///
/// On UniswapV4 only `ModifyLiquidity` changes tick liquidity. A positive
/// `liquidity_delta` (mint) adds at the lower tick and subtracts at the upper;
/// a negative one (burn) does the opposite.
pub fn event_to_tick_deltas(event: &PoolEvent) -> Vec<TickDelta> {
    match &event.kind {
        PoolEventKind::ModifyLiquidity { tick_lower, tick_upper, liquidity_delta } => {
            let amount = BigInt::from_str(liquidity_delta).unwrap_or_default();
            vec![
                TickDelta {
                    pool_id: event.pool_id.clone(),
                    tick_index: *tick_lower,
                    liquidity_net_delta: amount.clone(),
                },
                TickDelta {
                    pool_id: event.pool_id.clone(),
                    tick_index: *tick_upper,
                    liquidity_net_delta: -amount,
                },
            ]
        }
        PoolEventKind::Initialize { .. } |
        PoolEventKind::Swap { .. } |
        PoolEventKind::ProtocolFeeUpdated { .. } => vec![],
    }
}
