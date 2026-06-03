use anyhow::Result;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    lunarbase,
    modules::{
        config::{Config, PoolConfig},
        store_protocol_components::component_key,
    },
};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: tycho::BlockTransactionProtocolComponents,
    component_store: StoreGetProto<tycho::ProtocolComponent>,
) -> Result<tycho::BlockChanges> {
    let config = Config::parse(&params)?;
    let known_components = config
        .pools
        .iter()
        .filter_map(|pool| known_pool_component(pool, &component_store))
        .collect::<Vec<_>>();
    let mut builder = lunarbase::BlockChangesBuilder::new(known_components);

    for tx_components in new_components.tx_components.iter() {
        let Some(tx) = tx_components.tx.as_ref() else {
            continue;
        };
        for pool in tx_components
            .components
            .iter()
            .filter_map(|component| pool_by_component_id(&config, &component.id))
        {
            builder.register_component(
                tx,
                lunarbase::protocol_component(pool.pool, pool.token_x, pool.token_y),
                pool.bootstrap_state,
            );
        }
    }

    for tx in block.transactions() {
        for (log, _) in tx.logs_with_calls() {
            let Some(pool) = pool_by_address(&config, &log.address) else {
                continue;
            };
            if !builder.is_known_component(&pool.component_id()) {
                continue;
            }

            let indexed_tx: tycho::Transaction = tx.into();
            if let Err(err) = builder.apply_log(
                &indexed_tx,
                log,
                lunarbase::EventApplyContext { block_number: block.number },
                pool.token_x,
                pool.token_y,
            ) {
                substreams::log::info!("failed to apply LunarBase log: {:?}", err);
            }
        }
    }

    Ok(builder.finish((&block).into()))
}

fn known_pool_component(
    pool: &PoolConfig,
    component_store: &StoreGetProto<tycho::ProtocolComponent>,
) -> Option<String> {
    let component_id = pool.component_id();
    component_store
        .get_last(component_key(&component_id))
        .map(|_| component_id)
}

fn pool_by_component_id<'a>(config: &'a Config, component_id: &str) -> Option<&'a PoolConfig> {
    config
        .pools
        .iter()
        .find(|pool| pool.component_id() == component_id)
}

fn pool_by_address<'a>(config: &'a Config, address: &[u8]) -> Option<&'a PoolConfig> {
    config
        .pools
        .iter()
        .find(|pool| address == pool.pool)
}
