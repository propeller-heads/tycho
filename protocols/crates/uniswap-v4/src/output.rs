use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct AttributeUpdate {
    pub pool_id: Vec<u8>,
    pub name: String,
    pub value: Vec<u8>,
}

/// Computes plain attribute updates for an event.
///
/// `Swap` updates `sqrt_price_x96` and `tick`. `ProtocolFeeUpdated` splits the
/// packed uint24 fee into the lower 12 bits (`protocol_fees/zero2one`) and the
/// next 12 bits (`protocol_fees/one2zero`). `Initialize` is handled at pool
/// creation, not here.
pub fn event_to_attribute_updates(event: &PoolEvent) -> Vec<AttributeUpdate> {
    match &event.kind {
        PoolEventKind::Swap { sqrt_price, tick, .. } => vec![
            AttributeUpdate {
                pool_id: event.pool_id.clone(),
                name: "sqrt_price_x96".to_string(),
                value: BigInt::from_str(sqrt_price)
                    .unwrap_or_default()
                    .to_signed_bytes_be(),
            },
            AttributeUpdate {
                pool_id: event.pool_id.clone(),
                name: "tick".to_string(),
                value: BigInt::from(*tick).to_signed_bytes_be(),
            },
        ],
        PoolEventKind::ProtocolFeeUpdated { protocol_fee } => {
            let zero2one = protocol_fee & 0xFFF;
            let one2zero = (protocol_fee >> 12) & 0xFFF;
            vec![
                AttributeUpdate {
                    pool_id: event.pool_id.clone(),
                    name: "protocol_fees/zero2one".to_string(),
                    value: BigInt::from(zero2one).to_signed_bytes_be(),
                },
                AttributeUpdate {
                    pool_id: event.pool_id.clone(),
                    name: "protocol_fees/one2zero".to_string(),
                    value: BigInt::from(one2zero).to_signed_bytes_be(),
                },
            ]
        }
        PoolEventKind::Initialize { .. } | PoolEventKind::ModifyLiquidity { .. } => vec![],
    }
}
