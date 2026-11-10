use substreams::store::{StoreGet, StoreGetString};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log},
    Event,
};

use crate::{
    abi::plugin::events::{CallGasBudgetSet, Incentive, MevxRouterSet, MinGasLeftSet},
    modules::store_pool_plugins::plugin_pool_key,
    pb::nami::v4::{
        events::{
            pool_event::{self, Typ},
            PoolEvent,
        },
        Events,
    },
};

/// Settings a pool's plugin emits from its own address, attributed to the pool.
///
/// The plugin resolves to its pool through the store's value at the end of the block: for a pool
/// created after the plugin upgrade that added these settings, the plugin's initializer emits
/// them in the creation transaction before the pool's `Plugin` event names the plugin. Only the
/// pool's current plugin counts, so a plugin the pool has replaced no longer reaches it.
#[substreams::handlers::map]
pub fn map_plugin_events(block: eth::Block, pool_plugins_store: StoreGetString) -> Events {
    let pool_events = block
        .transactions()
        .flat_map(|tx| {
            tx.logs_with_calls()
                .filter_map(|(log, _)| {
                    let typ = maybe_plugin_event(log)?;
                    let pool = pool_plugins_store.get_last(plugin_pool_key(&log.address))?;
                    if pool_plugins_store.get_last(&pool)? != hex::encode(&log.address) {
                        return None;
                    }
                    Some(PoolEvent {
                        log_ordinal: log.ordinal,
                        pool_address: hex::decode(pool).ok()?,
                        transaction: Some(tx.into()),
                        typ: Some(typ),
                    })
                })
        })
        .collect();

    Events { pool_events }
}

fn maybe_plugin_event(log: &Log) -> Option<Typ> {
    if let Some(event) = MevxRouterSet::match_and_decode(log) {
        return Some(Typ::MevxRouter(pool_event::MevxRouter { router: event.new_mevx_router }));
    }
    if let Some(event) = MinGasLeftSet::match_and_decode(log) {
        return Some(Typ::MinGasLeft(pool_event::MinGasLeft {
            min_gas_left: event.new_min_gas_left.to_bytes_be().1,
        }));
    }
    if let Some(event) = CallGasBudgetSet::match_and_decode(log) {
        return Some(Typ::CallGasBudget(pool_event::CallGasBudget {
            call_gas_budget: event
                .new_call_gas_budget
                .to_bytes_be()
                .1,
        }));
    }
    if let Some(event) = Incentive::match_and_decode(log) {
        return Some(Typ::Incentive(pool_event::Incentive { incentive: event.new_incentive }));
    }
    None
}
