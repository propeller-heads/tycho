use crate::{
    modules::utils::{
        fee_module_pools_key, is_first_dynamic_fee_config_event, DynamicFeeEvent, Params,
    },
    pb::tycho::evm::aerodrome::Pool,
};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{Appender, StoreAppend, StoreGet, StoreGetProto},
};
use substreams_ethereum::pb::eth::v2 as eth;
use substreams_helper::hex::Hexable;

/// Records, per fee module, the pools it configured, so a module-wide setting such as its TWAP
/// window can be emitted on each of them.
#[substreams::handlers::store]
pub fn store_dynamic_fee_module_pools(
    params: String,
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
    dynamic_fee_config_deltas: StoreDeltas,
    store: StoreAppend<String>,
) {
    let params = Params::parse_from_query(&params).expect("Invalid module parameters");
    if !params.processes_dynamic_fee_config(block.number) {
        return;
    }
    let dynamic_fee_modules = params
        .dynamic_fee_modules
        .iter()
        .map(|module| hex::decode(module).expect("Invalid dynamic_fee_module hex"))
        .collect::<Vec<_>>();

    for transaction in block.transactions() {
        for (log, _) in transaction.logs_with_calls() {
            if !dynamic_fee_modules.contains(&log.address) {
                continue;
            }
            let Some(event) = DynamicFeeEvent::match_and_decode(log) else {
                continue;
            };
            let pool = event.pool();
            // Same filter as map_protocol_changes, which only snapshots indexed pools.
            if pools_store
                .get_at(log.ordinal, format!("Pool:{}", pool.to_hex()))
                .is_some() &&
                is_first_dynamic_fee_config_event(&dynamic_fee_config_deltas, log.ordinal, pool)
            {
                store.append(log.ordinal, fee_module_pools_key(&log.address), pool.to_hex());
            }
        }
    }
}
