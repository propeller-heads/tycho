use std::str::FromStr;

use anyhow::Ok;
use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas};

use crate::pb::uniswap::v3::{
    events::{pool_event, PoolEvent},
    Events,
};
use substreams::{
    scalar::BigInt,
    store::{StoreAddBigInt, StoreNew},
};

#[substreams::handlers::map]
pub fn map_balance_changes(events: Events) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let balance_deltas = events
        .pool_events
        .into_iter()
        .flat_map(event_to_balance_deltas)
        .collect();

    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}

fn event_to_balance_deltas(event: PoolEvent) -> Vec<BalanceDelta> {
    let address = format!("0x{}", event.pool_address)
        .as_bytes()
        .to_vec();
    match event.r#type.unwrap() {
        pool_event::Type::Mint(e) => vec![
            BalanceDelta {
                token: hex::decode(event.token0).unwrap(),
                delta: BigInt::from_str(&e.amount_0)
                    .unwrap()
                    .to_signed_bytes_be(),
                component_id: address.clone(),
                ord: event.log_ordinal,
                tx: event
                    .transaction
                    .as_ref()
                    .map(Into::into),
            },
            BalanceDelta {
                token: hex::decode(event.token1).unwrap(),
                delta: BigInt::from_str(&e.amount_1)
                    .unwrap()
                    .to_signed_bytes_be(),
                component_id: address,
                ord: event.log_ordinal,
                tx: event.transaction.map(Into::into),
            },
        ],
        pool_event::Type::Collect(e) => vec![
            BalanceDelta {
                token: hex::decode(event.token0).unwrap(),
                delta: BigInt::from_str(&e.amount_0)
                    .unwrap()
                    .neg()
                    .to_signed_bytes_be(),
                component_id: address.clone(),
                ord: event.log_ordinal,
                tx: event
                    .transaction
                    .as_ref()
                    .map(Into::into),
            },
            BalanceDelta {
                token: hex::decode(event.token1).unwrap(),
                delta: BigInt::from_str(&e.amount_1)
                    .unwrap()
                    .neg()
                    .to_signed_bytes_be(),
                component_id: address,
                ord: event.log_ordinal,
                tx: event.transaction.map(Into::into),
            },
        ],
        //Burn balance changes are accounted for in the Collect event.
        pool_event::Type::Burn(_) => vec![],
        pool_event::Type::Swap(e) => {
            vec![
                BalanceDelta {
                    token: hex::decode(event.token0).unwrap(),
                    delta: BigInt::from_str(&e.amount_0)
                        .unwrap()
                        .to_signed_bytes_be(),
                    component_id: address.clone(),
                    ord: event.log_ordinal,
                    tx: event
                        .transaction
                        .as_ref()
                        .map(Into::into),
                },
                BalanceDelta {
                    token: hex::decode(event.token1).unwrap(),
                    delta: BigInt::from_str(&e.amount_1)
                        .unwrap()
                        .to_signed_bytes_be(),
                    component_id: address,
                    ord: event.log_ordinal,
                    tx: event.transaction.map(Into::into),
                },
            ]
        }
        pool_event::Type::Flash(e) => vec![
            BalanceDelta {
                token: hex::decode(event.token0).unwrap(),
                delta: BigInt::from_str(&e.paid_0)
                    .unwrap()
                    .to_signed_bytes_be(),
                component_id: address.clone(),
                ord: event.log_ordinal,
                tx: event
                    .transaction
                    .as_ref()
                    .map(Into::into),
            },
            BalanceDelta {
                token: hex::decode(event.token1).unwrap(),
                delta: BigInt::from_str(&e.paid_1)
                    .unwrap()
                    .to_signed_bytes_be(),
                component_id: address,
                ord: event.log_ordinal,
                tx: event.transaction.map(Into::into),
            },
        ],
        pool_event::Type::CollectProtocol(e) => {
            vec![
                BalanceDelta {
                    token: hex::decode(event.token0).unwrap(),
                    delta: BigInt::from_str(&e.amount_0)
                        .unwrap()
                        .neg()
                        .to_signed_bytes_be(),
                    component_id: address.clone(),
                    ord: event.log_ordinal,
                    tx: event
                        .transaction
                        .as_ref()
                        .map(Into::into),
                },
                BalanceDelta {
                    token: hex::decode(event.token1).unwrap(),
                    delta: BigInt::from_str(&e.amount_1)
                        .unwrap()
                        .neg()
                        .to_signed_bytes_be(),
                    component_id: address,
                    ord: event.log_ordinal,
                    tx: event.transaction.map(Into::into),
                },
            ]
        }
        _ => vec![],
    }
}
