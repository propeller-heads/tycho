use substreams::store::{StoreNew, StoreSet, StoreSetString};

use crate::pb::nami::v4::{events::pool_event::Typ, Events};

/// Store key of the reverse lookup, plugin address to pool address (both hex).
pub fn plugin_pool_key(plugin: &[u8]) -> String {
    format!("plugin:{}", hex::encode(plugin))
}

/// Store key of a pool's current plugin config (decimal), keyed by pool address (hex).
pub fn pool_config_key(pool: &[u8]) -> String {
    format!("config:{}", hex::encode(pool))
}

/// Tracks each pool's current plugin address (hex), keyed by pool address (hex), the reverse,
/// keyed by `plugin_pool_key`, and each pool's current plugin config, keyed by `pool_config_key`.
#[substreams::handlers::store]
pub fn store_pool_plugins(events: Events, store: StoreSetString) {
    for event in events.pool_events {
        match event.typ {
            Some(Typ::Plugin(plugin)) => {
                let pool = hex::encode(&event.pool_address);
                store.set(event.log_ordinal, &pool, &hex::encode(&plugin.plugin));
                store.set(event.log_ordinal, plugin_pool_key(&plugin.plugin), &pool);
            }
            Some(Typ::PluginConfig(config)) => {
                store.set(
                    event.log_ordinal,
                    pool_config_key(&event.pool_address),
                    &config.plugin_config.to_string(),
                );
            }
            _ => {}
        }
    }
}
