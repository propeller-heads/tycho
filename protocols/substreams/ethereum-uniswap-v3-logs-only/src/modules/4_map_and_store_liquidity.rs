use substreams::store::{
    StoreGet, StoreGetInt64, StoreSet, StoreSetInt64, StoreSetSum, StoreSetSumBigInt,
};

use crate::pb::uniswap::v3::{Events, LiquidityChange, LiquidityChangeType, LiquidityChanges};

use substreams::{scalar::BigInt, store::StoreNew};

use uniswap_v3_core::events::PoolEvent as CorePoolEvent;

#[substreams::handlers::store]
pub fn store_pool_current_tick(events: Events, store: StoreSetInt64) {
    events
        .pool_events
        .into_iter()
        .for_each(|proto_event| {
            let pool_address = proto_event.pool_address.clone();
            let ordinal = proto_event.log_ordinal;
            let core_event = CorePoolEvent::from(&proto_event);
            if let Some(tick) = uniswap_v3_core::liquidity::event_to_current_tick(&core_event) {
                // tick from core is i64; store expects i64 reference
                store.set(ordinal, format!("pool:{pool_address}"), &tick)
            }
        });
}

#[substreams::handlers::map]
pub fn map_liquidity_changes(
    events: Events,
    pools_current_tick_store: StoreGetInt64,
) -> Result<LiquidityChanges, anyhow::Error> {
    let mut changes = events
        .pool_events
        .into_iter()
        .filter_map(|proto_event| {
            let current_tick = pools_current_tick_store
                .get_at(proto_event.log_ordinal, format!("pool:{0}", proto_event.pool_address))
                .unwrap_or(0);
            let ordinal = proto_event.log_ordinal;
            let transaction = proto_event.transaction.clone();
            let core_event = CorePoolEvent::from(&proto_event);
            let delta =
                uniswap_v3_core::liquidity::event_to_liquidity_delta(current_tick, &core_event)?;
            Some(LiquidityChange { ordinal, transaction, ..delta.into() })
        })
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
                    format!("pool:{0}", hex::encode(&changes.pool_address)),
                    BigInt::from_signed_bytes_be(&changes.value),
                );
            }
            LiquidityChangeType::Absolute => {
                store.set(
                    changes.ordinal,
                    format!("pool:{0}", hex::encode(&changes.pool_address)),
                    BigInt::from_signed_bytes_be(&changes.value),
                );
            }
        });
}
