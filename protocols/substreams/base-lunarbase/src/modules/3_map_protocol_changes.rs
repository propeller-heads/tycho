use std::collections::{HashMap, HashSet};

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
    config.validate_bootstrap_parent(
        block.number,
        block
            .header
            .as_ref()
            .map(|header| header.parent_hash.as_slice()),
    )?;
    let mut known_components = config
        .pools
        .iter()
        .filter_map(|pool| known_pool_component(pool, &component_store))
        .collect::<HashSet<_>>();
    let mut transaction_changes = HashMap::<u64, tycho::TransactionChangesBuilder>::new();

    for tx_components in new_components.tx_components.iter() {
        let Some(tx) = tx_components.tx.as_ref() else {
            continue;
        };
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| tycho::TransactionChangesBuilder::new(tx));

        for component in tx_components.components.iter() {
            let Some(pool) = pool_by_component_id(&config, &component.id) else {
                continue;
            };
            let component = lunarbase::protocol_component(
                pool.pool,
                pool.token_x,
                pool.token_y,
                config.quote_caller,
            );
            known_components.insert(component.id.clone());
            builder.add_protocol_component(&component);
            let initial_state = match config.bootstrap_states.get(&pool.pool) {
                Some(snapshot) => {
                    for balance in
                        snapshot.balance_changes(&component.id, pool.token_x, pool.token_y)
                    {
                        builder.add_balance_change(&balance);
                    }
                    snapshot.entity_change(&component.id)
                }
                None => lunarbase::indexed::initial_entity_change(&component.id),
            };
            builder.add_entity_change(&initial_state);
        }
    }

    for tx in block.transactions() {
        for (log, _) in tx.logs_with_calls() {
            let Some(pool) = pool_by_address(&config, &log.address) else {
                continue;
            };
            let component_id = pool.component_id();
            if !known_components.contains(&component_id) {
                continue;
            }

            let event = match lunarbase::events::decode_lunarbase_state_log(log, &config.quote_caller) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(err) => return Err(anyhow::anyhow!(
                    "failed to decode LunarBase state for {component_id} at block {}, transaction 0x{}, log {}: {err}",
                    block.number,
                    hex::encode(&tx.hash),
                    log.index,
                )),
            };
            let tx: tycho::Transaction = tx.into();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| tycho::TransactionChangesBuilder::new(&tx));
            builder.add_entity_change(&lunarbase::indexed::entity_change_for_event(
                &component_id,
                &event,
                block.number,
            ));
            for balance_change in lunarbase::indexed::balance_changes_for_event(
                &component_id,
                &pool.token_x,
                &pool.token_y,
                &event,
            ) {
                builder.add_balance_change(&balance_change);
            }
        }
    }

    let mut changes = transaction_changes
        .into_values()
        .filter_map(tycho::TransactionChangesBuilder::build)
        .collect::<Vec<_>>();
    changes.sort_unstable_by_key(|changes| {
        changes
            .tx
            .as_ref()
            .map(|tx| tx.index)
            .unwrap_or_default()
    });

    Ok(tycho::BlockChanges { block: Some((&block).into()), changes, ..Default::default() })
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
