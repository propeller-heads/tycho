use substreams::store::{StoreGet, StoreGetInt64};

use substreams_helper::hex::Hexable;

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::{pool_log::Event, PoolLog},
    BlockTransactionEvents, ChangeType, LiquidityChange, LiquidityChanges,
};

#[substreams::handlers::map]
pub fn map_liquidity_changes(
    block_tx_events: BlockTransactionEvents,
    current_tick_store: StoreGetInt64,
) -> LiquidityChanges {
    LiquidityChanges {
        changes: block_tx_events
            .block_transaction_events
            .into_iter()
            .flat_map(|tx_events| {
                let current_tick_store = &current_tick_store;

                tx_events
                    .pool_logs
                    .into_iter()
                    .filter_map(move |log| {
                        maybe_liquidity_change(&log, current_tick_store).map(|partial| {
                            LiquidityChange {
                                change_type: partial.change_type.into(),
                                pool_id: log.pool_id,
                                value: partial.value,
                                ordinal: log.ordinal,
                                transaction: tx_events.transaction.clone(),
                            }
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

fn maybe_liquidity_change(
    log: &PoolLog,
    current_tick_store: &StoreGetInt64,
) -> Option<PartialLiquidityChange> {
    match log.event.as_ref().unwrap() {
        Event::Swapped(swapped) => Some(PartialLiquidityChange {
            value: swapped.liquidity_after.clone(),
            change_type: ChangeType::Absolute,
        }),
        Event::PositionUpdated(position_updated) => {
            let current_tick = current_tick_store
                .get_at(log.ordinal, format!("pool:{0}", log.pool_id.to_hex()))
                .expect("pool should have active tick when initialized");

            (current_tick >= position_updated.lower.into() &&
                current_tick < position_updated.upper.into())
            .then(|| PartialLiquidityChange {
                value: position_updated.liquidity_delta.clone(),
                change_type: ChangeType::Delta,
            })
        }
        _ => None,
    }
}
