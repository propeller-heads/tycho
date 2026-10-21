use substreams::scalar::BigInt;

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::pool_log::Event, BlockTransactionEvents,
    TickDelta, TickDeltas,
};

#[substreams::handlers::map]
pub fn map_tick_deltas(block_tx_events: BlockTransactionEvents) -> TickDeltas {
    TickDeltas {
        deltas: block_tx_events
            .block_transaction_events
            .into_iter()
            .flat_map(|tx_events| {
                let tx = tx_events.transaction;

                tx_events
                    .pool_logs
                    .into_iter()
                    .flat_map(move |log| {
                        let tx = tx.clone();

                        tick_deltas(log.event.unwrap())
                            .into_iter()
                            .map(move |partial| TickDelta {
                                liquidity_net_delta: partial.liquidity_net_delta,
                                pool_id: log.pool_id.clone(),
                                tick_index: partial.tick_index,
                                ordinal: log.ordinal,
                                transaction: tx.clone(),
                            })
                    })
            })
            .collect(),
    }
}

struct PartialTickDelta {
    tick_index: i32,
    liquidity_net_delta: Vec<u8>,
}

fn tick_deltas(ev: Event) -> Vec<PartialTickDelta> {
    match ev {
        Event::PositionUpdated(position_updated) => {
            vec![
                PartialTickDelta {
                    tick_index: position_updated.lower,
                    liquidity_net_delta: position_updated.liquidity_delta.clone(),
                },
                PartialTickDelta {
                    tick_index: position_updated.upper,
                    liquidity_net_delta: BigInt::from_signed_bytes_be(
                        &position_updated.liquidity_delta,
                    )
                    .neg()
                    .to_signed_bytes_be(),
                },
            ]
        }
        _ => vec![],
    }
}
