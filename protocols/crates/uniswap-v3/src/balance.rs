use std::str::FromStr;

use num_bigint::BigInt;
use tycho_substreams::pb::tycho::evm::v1::BalanceDelta as ProtoBalanceDelta;

use crate::events::{PoolEvent, PoolEventKind};

pub struct BalanceDelta {
    pub token: Vec<u8>,
    pub component_id: Vec<u8>,
    pub delta: BigInt,
}

impl From<BalanceDelta> for ProtoBalanceDelta {
    fn from(d: BalanceDelta) -> Self {
        Self {
            token: d.token,
            delta: d.delta.to_signed_bytes_be(),
            component_id: format!("0x{}", hex::encode(&d.component_id)).into_bytes(),
            ord: 0,
            tx: None,
        }
    }
}

pub fn event_to_balance_deltas(event: &PoolEvent) -> Vec<BalanceDelta> {
    let component_id = event.pool_address.clone();

    match &event.kind {
        PoolEventKind::Mint { amount0, amount1, .. } => vec![
            BalanceDelta {
                token: event.token0.clone(),
                component_id: component_id.clone(),
                delta: BigInt::from_str(amount0).unwrap_or_default(),
            },
            BalanceDelta {
                token: event.token1.clone(),
                component_id,
                delta: BigInt::from_str(amount1).unwrap_or_default(),
            },
        ],
        PoolEventKind::Collect { amount0, amount1 } => vec![
            BalanceDelta {
                token: event.token0.clone(),
                component_id: component_id.clone(),
                delta: -BigInt::from_str(amount0).unwrap_or_default(),
            },
            BalanceDelta {
                token: event.token1.clone(),
                component_id,
                delta: -BigInt::from_str(amount1).unwrap_or_default(),
            },
        ],
        // Burn balance changes are accounted for in the Collect event.
        PoolEventKind::Burn { .. } => vec![],
        PoolEventKind::Swap { amount0, amount1, .. } => vec![
            BalanceDelta {
                token: event.token0.clone(),
                component_id: component_id.clone(),
                delta: BigInt::from_str(amount0).unwrap_or_default(),
            },
            BalanceDelta {
                token: event.token1.clone(),
                component_id,
                delta: BigInt::from_str(amount1).unwrap_or_default(),
            },
        ],
        PoolEventKind::Flash { paid0, paid1 } => vec![
            BalanceDelta {
                token: event.token0.clone(),
                component_id: component_id.clone(),
                delta: BigInt::from_str(paid0).unwrap_or_default(),
            },
            BalanceDelta {
                token: event.token1.clone(),
                component_id,
                delta: BigInt::from_str(paid1).unwrap_or_default(),
            },
        ],
        PoolEventKind::CollectProtocol { amount0, amount1 } => vec![
            BalanceDelta {
                token: event.token0.clone(),
                component_id: component_id.clone(),
                delta: -BigInt::from_str(amount0).unwrap_or_default(),
            },
            BalanceDelta {
                token: event.token1.clone(),
                component_id,
                delta: -BigInt::from_str(amount1).unwrap_or_default(),
            },
        ],
        _ => vec![],
    }
}
