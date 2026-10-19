use substreams::scalar::BigInt;

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::pool_log::Event, BlockTransactionEvents,
    OrderSaleRateDelta, OrderSaleRateDeltas,
};

#[substreams::handlers::map]
pub fn map_order_sale_rate_deltas(block_tx_events: BlockTransactionEvents) -> OrderSaleRateDeltas {
    OrderSaleRateDeltas {
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

                        order_sale_rate_deltas(log.event.unwrap())
                            .into_iter()
                            .map(move |partial| OrderSaleRateDelta {
                                pool_id: log.pool_id.clone(),
                                time: partial.time,
                                sale_rate_delta: partial.sale_rate_delta,
                                is_token1: partial.is_token1,
                                ordinal: log.ordinal,
                                transaction: tx.clone(),
                            })
                    })
            })
            .collect(),
    }
}

struct PartialOrderSaleRateDelta {
    time: u64,
    sale_rate_delta: Vec<u8>,
    is_token1: bool,
}

fn order_sale_rate_deltas(ev: Event) -> Vec<PartialOrderSaleRateDelta> {
    match ev {
        Event::OrderUpdated(ev) => {
            let key = ev.order_key.unwrap();

            let is_token1 = key.sell_token > key.buy_token;
            let sale_rate_delta = ev.sale_rate_delta;

            vec![
                PartialOrderSaleRateDelta {
                    time: key.start_time,
                    sale_rate_delta: sale_rate_delta.clone(),
                    is_token1,
                },
                PartialOrderSaleRateDelta {
                    time: key.end_time,
                    sale_rate_delta: BigInt::from_signed_bytes_be(&sale_rate_delta)
                        .neg()
                        .to_signed_bytes_be(),
                    is_token1,
                },
            ]
        }
        _ => vec![],
    }
}
