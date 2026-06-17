use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

/// An absolute component balance for a single token, encoded as signed big-endian bytes
/// to match the substreams output.
pub struct AbsoluteBalance {
    pub token: Vec<u8>,
    pub balance: Vec<u8>,
}

/// Computes the absolute component balances an event produces.
///
/// A `Sync` event reports the pool's reserves directly, so the component balances are the
/// reserves themselves: `token0 = reserve0`, `token1 = reserve1`. No accumulation is needed.
pub fn event_to_balances(event: &PoolEvent) -> Vec<AbsoluteBalance> {
    match &event.kind {
        PoolEventKind::Sync { reserve0, reserve1 } => vec![
            AbsoluteBalance {
                token: event.token0.clone(),
                balance: BigInt::from_str(reserve0)
                    .unwrap_or_default()
                    .to_signed_bytes_be(),
            },
            AbsoluteBalance {
                token: event.token1.clone(),
                balance: BigInt::from_str(reserve1)
                    .unwrap_or_default()
                    .to_signed_bytes_be(),
            },
        ],
    }
}
