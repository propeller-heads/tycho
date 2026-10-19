use substreams::store::{StoreGet, StoreGetInt64, StoreGetProto};

use substreams_helper::hex::Hexable;

use crate::{
    details_store::get_pool_details,
    pb::ekubo::{
        block_transaction_events::transaction_events::{pool_log::Event, PoolLog},
        ActiveLiquidityChange, ActiveLiquidityChanges, BlockTransactionEvents, ChangeType,
        PoolDetails,
    },
};

#[substreams::handlers::map]
pub fn map_active_liquidity_changes(
    block_tx_events: BlockTransactionEvents,
    pool_details_store: StoreGetProto<PoolDetails>,
    current_tick_store: StoreGetInt64,
) -> ActiveLiquidityChanges {
    ActiveLiquidityChanges {
        changes: block_tx_events
            .block_transaction_events
            .into_iter()
            .flat_map(|tx_events| {
                let (pool_details_store, current_tick_store) =
                    (&pool_details_store, &current_tick_store);

                tx_events
                    .pool_logs
                    .into_iter()
                    .filter_map(move |log| {
                        maybe_active_liquidity_change(&log, pool_details_store, current_tick_store)
                            .map(|partial| ActiveLiquidityChange {
                                change_type: partial.change_type.into(),
                                pool_id: log.pool_id,
                                value: partial.value,
                                ordinal: log.ordinal,
                                transaction: tx_events.transaction.clone(),
                            })
                    })
            })
            .collect(),
    }
}

struct PartialLiquidityChange {
    value: Vec<u8>,
    change_type: ChangeType,
}

fn maybe_active_liquidity_change(
    log: &PoolLog,
    pool_details_store: &StoreGetProto<PoolDetails>,
    current_tick_store: &StoreGetInt64,
) -> Option<PartialLiquidityChange> {
    match log.event.as_ref().unwrap() {
        Event::Swapped(swapped) => Some(PartialLiquidityChange {
            value: swapped.liquidity_after.clone(),
            change_type: ChangeType::Absolute,
        }),
        Event::PositionUpdated(position_updated) => {
            let pool_id = log.pool_id.to_hex();

            let update_active_liquidity =
                if get_pool_details(pool_details_store, &pool_id)?.is_stableswap {
                    true
                } else {
                    let current_tick = current_tick_store
                        .get_at(log.ordinal, &pool_id)
                        .expect("pool should have active tick when initialized");

                    current_tick >= position_updated.lower.into() &&
                        current_tick < position_updated.upper.into()
                };

            update_active_liquidity.then(|| PartialLiquidityChange {
                value: position_updated.liquidity_delta.clone(),
                change_type: ChangeType::Delta,
            })
        }
        _ => None,
    }
}
