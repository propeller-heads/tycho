use crate::pb::ekubo::{
    block_transaction_events::transaction_events::{pool_log::Event, PoolLog},
    ActiveRateChange, ActiveRateChanges, BlockTransactionEvents, ChangeType,
};

#[substreams::handlers::map]
pub fn map_active_rate_changes(block_tx_events: BlockTransactionEvents) -> ActiveRateChanges {
    ActiveRateChanges {
        changes: block_tx_events
            .block_transaction_events
            .into_iter()
            .flat_map(|tx_events| {
                tx_events
                    .pool_logs
                    .into_iter()
                    .filter_map(move |log| {
                        maybe_rate_change(&log, block_tx_events.timestamp).map(|partial| {
                            ActiveRateChange {
                                change_type: partial.change_type.into(),
                                pool_id: log.pool_id,
                                token0_value: partial.token0_value,
                                token1_value: partial.token1_value,
                                ordinal: log.ordinal,
                                transaction: tx_events.transaction.clone(),
                            }
                        })
                    })
            })
            .collect(),
    }
}

struct PartialActiveRateChange {
    token0_value: Vec<u8>,
    token1_value: Vec<u8>,
    change_type: ChangeType,
}

fn maybe_rate_change(log: &PoolLog, timestamp: u64) -> Option<PartialActiveRateChange> {
    match log.event.as_ref().unwrap() {
        // For TWAMM this is `VirtualExecution`; for BoostedFees this is `FeesDonated`.
        Event::VirtualExecution(ev) => Some(PartialActiveRateChange {
            change_type: ChangeType::Absolute,
            token0_value: ev.token0_rate.clone(),
            token1_value: ev.token1_rate.clone(),
        }),
        // For TWAMM this is `OrderUpdated`; for BoostedFees this is `PoolBoosted`.
        Event::RateUpdated(ev) => {
            // An execution always happens before a rate update
            let last_time = timestamp;

            (last_time >= ev.start_time && last_time < ev.end_time).then(|| {
                PartialActiveRateChange {
                    change_type: ChangeType::Delta,
                    token0_value: ev.token0_rate_delta.clone(),
                    token1_value: ev.token1_rate_delta.clone(),
                }
            })
        }
        _ => None,
    }
}
