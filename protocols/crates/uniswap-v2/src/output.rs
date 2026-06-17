use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct AttributeUpdate {
    pub pool_address: Vec<u8>,
    pub name: String,
    pub value: Vec<u8>,
}

/// Computes the protocol-state attribute updates an event produces.
///
/// A `Sync` event sets the pool's `reserve0` and `reserve1` to the absolute values it
/// carries, encoded as signed big-endian bytes to match the substreams output.
pub fn event_to_attribute_updates(event: &PoolEvent) -> Vec<AttributeUpdate> {
    match &event.kind {
        PoolEventKind::Sync { reserve0, reserve1 } => vec![
            AttributeUpdate {
                pool_address: event.pool_address.clone(),
                name: "reserve0".to_string(),
                value: BigInt::from_str(reserve0)
                    .unwrap_or_default()
                    .to_signed_bytes_be(),
            },
            AttributeUpdate {
                pool_address: event.pool_address.clone(),
                name: "reserve1".to_string(),
                value: BigInt::from_str(reserve1)
                    .unwrap_or_default()
                    .to_signed_bytes_be(),
            },
        ],
    }
}
