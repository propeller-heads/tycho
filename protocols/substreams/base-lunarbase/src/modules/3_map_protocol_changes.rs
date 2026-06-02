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
    tycho_mapper::to_tycho_block_changes,
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
        .filter_map(|pool| known_pool_component(pool, &new_components, &component_store))
        .collect::<Vec<_>>();
    let mut builder = lunarbase::BlockChangesBuilder::new(known_components);

    for tx_components in new_components.tx_components.iter() {
        let Some(tx) = tx_components
            .tx
            .as_ref()
            .and_then(indexed_tx_from_tycho_tx)
        else {
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

            let Some(indexed_tx) = indexed_tx_from_eth_tx(tx) else {
                continue;
            };
            if let Err(err) = builder.apply_log(
                indexed_tx,
                log,
                lunarbase::EventApplyContext {
                    block_number: block.number,
                    tycho_executor: config.tycho_executor,
                },
                pool.token_x,
                pool.token_y,
            ) {
                substreams::log::info!("failed to apply LunarBase log: {:?}", err);
            }
        }
    }

    Ok(to_tycho_block_changes((&block).into(), builder.finish()))
}

fn known_pool_component(
    pool: &PoolConfig,
    new_components: &tycho::BlockTransactionProtocolComponents,
    component_store: &StoreGetProto<tycho::ProtocolComponent>,
) -> Option<String> {
    let component_id = pool.component_id();
    let known = pool.bootstrap_block.is_none() ||
        component_store
            .get_last(component_key(&component_id))
            .is_some() ||
        new_components
            .tx_components
            .iter()
            .flat_map(|tx| tx.components.iter())
            .any(|component| component.id == component_id);
    known.then_some(component_id)
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

fn indexed_tx_from_eth_tx(tx: &eth::v2::TransactionTrace) -> Option<lunarbase::IndexedTransaction> {
    Some(lunarbase::IndexedTransaction {
        hash: fixed_32(&tx.hash)?,
        from: fixed_20(&tx.from)?,
        to: fixed_20(&tx.to)?,
        index: tx.index.into(),
    })
}

fn indexed_tx_from_tycho_tx(tx: &tycho::Transaction) -> Option<lunarbase::IndexedTransaction> {
    Some(lunarbase::IndexedTransaction {
        hash: fixed_32(&tx.hash)?,
        from: fixed_20(&tx.from)?,
        to: fixed_20(&tx.to)?,
        index: tx.index,
    })
}

fn fixed_20(value: &[u8]) -> Option<[u8; 20]> {
    value.try_into().ok()
}

fn fixed_32(value: &[u8]) -> Option<[u8; 32]> {
    value.try_into().ok()
}
