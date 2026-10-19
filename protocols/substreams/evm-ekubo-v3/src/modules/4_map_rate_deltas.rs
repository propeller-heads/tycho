use substreams::scalar::BigInt;

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::pool_log::Event, BlockTransactionEvents,
    RateDelta, RateDeltas,
};

#[substreams::handlers::map]
pub fn map_rate_deltas(block_tx_events: BlockTransactionEvents) -> RateDeltas {
    RateDeltas {
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

                        rate_deltas(log.event.unwrap())
                            .into_iter()
                            .map(move |partial| RateDelta {
                                pool_id: log.pool_id.clone(),
                                time: partial.time,
                                rate_delta: partial.rate_delta,
                                is_token1: partial.is_token1,
                                ordinal: log.ordinal,
                                transaction: tx.clone(),
                            })
                    })
            })
            .collect(),
    }
}

struct PartialRateDelta {
    time: u64,
    rate_delta: Vec<u8>,
    is_token1: bool,
}

fn rate_deltas(ev: Event) -> Vec<PartialRateDelta> {
    match ev {
        Event::RateUpdated(ev) => {
            let mut deltas = Vec::with_capacity(2);

            if !ev.token0_rate_delta.is_empty() {
                deltas.extend([
                    PartialRateDelta {
                        time: ev.start_time,
                        rate_delta: ev.token0_rate_delta.clone(),
                        is_token1: false,
                    },
                    PartialRateDelta {
                        time: ev.end_time,
                        rate_delta: BigInt::from_signed_bytes_be(&ev.token0_rate_delta)
                            .neg()
                            .to_signed_bytes_be(),
                        is_token1: false,
                    },
                ]);
            }

            if !ev.token1_rate_delta.is_empty() {
                deltas.extend([
                    PartialRateDelta {
                        time: ev.start_time,
                        rate_delta: ev.token1_rate_delta.clone(),
                        is_token1: true,
                    },
                    PartialRateDelta {
                        time: ev.end_time,
                        rate_delta: BigInt::from_signed_bytes_be(&ev.token1_rate_delta)
                            .neg()
                            .to_signed_bytes_be(),
                        is_token1: true,
                    },
                ]);
            }

            deltas
        }
        _ => vec![],
    }
}
