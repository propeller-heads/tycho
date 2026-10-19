use std::str::FromStr;

use substreams::store::{StoreGet, StoreGetInt64, StoreSetSum, StoreSetSumBigInt};

use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChange, LiquidityChangeType, LiquidityChanges,
};

use substreams::scalar::BigInt;

use anyhow::Ok;
use substreams_helper::hex::Hexable;

#[substreams::handlers::map]
pub fn map_liquidity_changes(
    events: Events,
    pools_current_tick_store: StoreGetInt64,
) -> Result<LiquidityChanges, anyhow::Error> {
    let mut changes = events
        .pool_events
        .into_iter()
        .filter(PoolEvent::can_introduce_liquidity_changes)
        .map(|e| {
            (
                pools_current_tick_store
                    .get_at(e.log_ordinal, format!("pool:{0}", &e.pool_id))
                    .unwrap_or(0),
                e,
            )
        })
        .filter_map(|(current_tick, event)| event_to_liquidity_deltas(current_tick, event))
        .collect::<Vec<_>>();

    changes.sort_unstable_by_key(|l| l.ordinal);
    Ok(LiquidityChanges { changes })
}

#[substreams::handlers::store]
pub fn store_liquidity(ticks_deltas: LiquidityChanges, store: StoreSetSumBigInt) {
    ticks_deltas
        .changes
        .iter()
        .for_each(|changes| match changes.change_type() {
            LiquidityChangeType::Delta => {
                store.sum(
                    changes.ordinal,
                    format!("pool:{0}", &changes.pool_address.to_hex()),
                    BigInt::from_signed_bytes_be(&changes.value),
                );
            }
            LiquidityChangeType::Absolute => {
                store.set(
                    changes.ordinal,
                    format!("pool:{0}", &changes.pool_address.to_hex()),
                    BigInt::from_signed_bytes_be(&changes.value),
                );
            }
        });
}

fn event_to_liquidity_deltas(current_tick: i64, event: PoolEvent) -> Option<LiquidityChange> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(mod_liquidity) => {
            if current_tick >= mod_liquidity.tick_lower.into() &&
                current_tick < mod_liquidity.tick_upper.into()
            {
                Some(LiquidityChange {
                    pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    value: BigInt::from_str(&mod_liquidity.liquidity_delta)
                        .unwrap()
                        .to_signed_bytes_be(),
                    change_type: LiquidityChangeType::Delta.into(),
                    ordinal: event.log_ordinal,
                    transaction: Some(event.transaction.unwrap()),
                })
            } else {
                None
            }
        }
        pool_event::Type::Swap(swap) => Some(LiquidityChange {
            pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
            value: BigInt::from_str(&swap.liquidity)
                .unwrap()
                .to_signed_bytes_be(),
            change_type: LiquidityChangeType::Absolute.into(),
            ordinal: event.log_ordinal,
            transaction: Some(event.transaction.unwrap()),
        }),
        _ => None,
    }
}

impl PoolEvent {
    fn can_introduce_liquidity_changes(&self) -> bool {
        matches!(
            self.r#type.as_ref().unwrap(),
            pool_event::Type::ModifyLiquidity(_) | pool_event::Type::Swap(_)
        )
    }
}
