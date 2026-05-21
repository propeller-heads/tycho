use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas};

use crate::pb::uniswap::v3::Events;
use substreams::store::{StoreAddBigInt, StoreNew};

use uniswap_v3_core::events::PoolEvent as CorePoolEvent;

#[substreams::handlers::map]
pub fn map_balance_changes(events: Events) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let balance_deltas = events
        .pool_events
        .into_iter()
        .flat_map(|proto_event| {
            let ordinal = proto_event.log_ordinal;
            let tx = proto_event.transaction.clone();
            let core_event = CorePoolEvent::from(&proto_event);
            uniswap_v3_core::balance::event_to_balance_deltas(&core_event)
                .into_iter()
                .map(move |d| BalanceDelta {
                    ord: ordinal,
                    tx: tx.as_ref().map(Into::into),
                    ..d.into()
                })
        })
        .collect();

    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}
