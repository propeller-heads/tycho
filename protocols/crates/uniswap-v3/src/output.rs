use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct AttributeUpdate {
    pub pool_address: Vec<u8>,
    pub name: String,
    pub value: Vec<u8>,
    pub is_creation: bool,
}

pub fn event_to_attribute_updates(event: &PoolEvent) -> Vec<AttributeUpdate> {
    match &event.kind {
        PoolEventKind::Initialize { sqrt_price, tick } => {
            vec![
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "sqrt_price_x96".to_string(),
                    value: BigInt::from_str(sqrt_price)
                        .unwrap_or_default()
                        .to_signed_bytes_be(),
                    is_creation: false,
                },
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "tick".to_string(),
                    value: BigInt::from(*tick).to_signed_bytes_be(),
                    is_creation: false,
                },
            ]
        }
        PoolEventKind::Swap { sqrt_price, tick, .. } => {
            vec![
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "sqrt_price_x96".to_string(),
                    value: BigInt::from_str(sqrt_price)
                        .unwrap_or_default()
                        .to_signed_bytes_be(),
                    is_creation: false,
                },
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "tick".to_string(),
                    value: BigInt::from(*tick).to_signed_bytes_be(),
                    is_creation: false,
                },
            ]
        }
        PoolEventKind::SetFeeProtocol { fee0_new, fee1_new } => {
            vec![
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "protocol_fees/token0".to_string(),
                    value: BigInt::from(*fee0_new).to_signed_bytes_be(),
                    is_creation: false,
                },
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "protocol_fees/token1".to_string(),
                    value: BigInt::from(*fee1_new).to_signed_bytes_be(),
                    is_creation: false,
                },
            ]
        }
        _ => vec![],
    }
}
