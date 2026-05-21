use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct TickDelta {
    pub pool_address: Vec<u8>,
    pub tick_index: i32,
    pub liquidity_net_delta: BigInt,
}

pub fn event_to_tick_deltas(event: &PoolEvent) -> Vec<TickDelta> {
    match &event.kind {
        PoolEventKind::Mint { tick_lower, tick_upper, amount, .. } => {
            let amount_val = BigInt::from_str(amount).unwrap_or_default();
            vec![
                TickDelta {
                    pool_address: event.pool_address.clone(),
                    tick_index: *tick_lower,
                    liquidity_net_delta: amount_val.clone(),
                },
                TickDelta {
                    pool_address: event.pool_address.clone(),
                    tick_index: *tick_upper,
                    liquidity_net_delta: -amount_val,
                },
            ]
        }
        PoolEventKind::Burn { tick_lower, tick_upper, amount, .. } => {
            let amount_val = BigInt::from_str(amount).unwrap_or_default();
            vec![
                TickDelta {
                    pool_address: event.pool_address.clone(),
                    tick_index: *tick_lower,
                    liquidity_net_delta: -amount_val.clone(),
                },
                TickDelta {
                    pool_address: event.pool_address.clone(),
                    tick_index: *tick_upper,
                    liquidity_net_delta: amount_val,
                },
            ]
        }
        _ => vec![],
    }
}
