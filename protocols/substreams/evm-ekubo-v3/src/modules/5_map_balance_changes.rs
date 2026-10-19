use substreams::store::{StoreGet, StoreGetProto};
use substreams_helper::hex::Hexable;
use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas, Transaction};

use crate::{
    details_store::get_pool_details,
    pb::ekubo::{
        block_transaction_events::transaction_events::pool_log::Event, BlockTransactionEvents,
        PoolDetails,
    },
};

#[substreams::handlers::map]
fn map_balance_changes(
    block_tx_events: BlockTransactionEvents,
    store: StoreGetProto<PoolDetails>,
) -> BlockBalanceDeltas {
    BlockBalanceDeltas {
        balance_deltas: block_tx_events
            .block_transaction_events
            .into_iter()
            .flat_map(|tx_events| {
                let tx: Transaction = tx_events.transaction.unwrap().into();

                let store = &store;

                tx_events
                    .pool_logs
                    .into_iter()
                    .flat_map(move |log| {
                        let tx = tx.clone();

                        let (delta0, delta1) = maybe_balance_deltas(log.event.unwrap())?;
                        let pool_id = log.pool_id.to_hex();
                        let pool_details = get_pool_details(store, &pool_id)?;

                        Some(
                            [(delta0, pool_details.token0), (delta1, pool_details.token1)]
                                .into_iter()
                                .map(move |(delta, token)| BalanceDelta {
                                    ord: log.ordinal,
                                    tx: Some(tx.clone()),
                                    token,
                                    delta,
                                    component_id: pool_id.clone().into_bytes(),
                                }),
                        )
                    })
                    .flatten()
            })
            .collect(),
    }
}

fn maybe_balance_deltas(ev: Event) -> Option<(Vec<u8>, Vec<u8>)> {
    match ev {
        Event::Swapped(ev) => Some((ev.delta0, ev.delta1)),
        Event::PositionUpdated(ev) => Some((ev.delta0, ev.delta1)),
        _ => None,
    }
}
