use substreams::store::{StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString};

use crate::pb::algebra::integral::v1::{events::pool_event::Typ, Events};

/// Store key of the reverse lookup, plugin address to pool address (both hex). Every deployed
/// plugin serves one pool; a plugin shared by pools would need a list here.
pub fn plugin_pool_key(plugin: &[u8]) -> String {
    format!("plugin:{}", hex::encode(plugin))
}

/// A string store as the pool modules read it. Unit tests implement it over a map.
pub trait StringStore {
    /// The value of `key` after every write of the block up to `ordinal`.
    fn value_at(&self, ordinal: u64, key: &str) -> Option<String>;
}

impl StringStore for StoreGetString {
    fn value_at(&self, ordinal: u64, key: &str) -> Option<String> {
        self.get_at(ordinal, key)
    }
}

/// A write to a string store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreWrite {
    pub ordinal: u64,
    pub key: String,
    pub value: String,
}

/// A stored plugin address, or `None` when the store has no entry or holds the zero address.
pub fn decode_plugin(stored: Option<String>) -> Option<Vec<u8>> {
    stored
        .and_then(|plugin| hex::decode(plugin).ok())
        .filter(|plugin| plugin.iter().any(|byte| *byte != 0))
}

/// The plugin `pool` names after every event of the block up to `ordinal`, if any.
pub fn plugin_at(pool_plugins: &impl StringStore, ordinal: u64, pool: &[u8]) -> Option<Vec<u8>> {
    decode_plugin(pool_plugins.value_at(ordinal, &hex::encode(pool)))
}

/// A `Plugin` event that changes the plugin a pool names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSwitch {
    /// The plugin the pool named before the event, if any.
    pub old: Option<Vec<u8>>,
    /// The plugin it names from the event on, if any.
    pub new: Option<Vec<u8>>,
}

/// What the `Plugin` event at `ordinal`, naming `named`, changes on `pool`, or `None` when it
/// names the plugin the pool already names. The one place that decides whether such an event
/// changes anything the pool carries.
pub fn plugin_switch(
    pool_plugins: &impl StringStore,
    ordinal: u64,
    pool: &[u8],
    named: &[u8],
) -> Option<PluginSwitch> {
    let old = plugin_at(pool_plugins, ordinal.saturating_sub(1), pool);
    let new = decode_plugin(Some(hex::encode(named)));
    (old != new).then_some(PluginSwitch { old, new })
}

/// What the block's events write: each pool's current plugin address (hex), keyed by pool
/// address (hex), and the reverse, keyed by `plugin_pool_key`.
pub fn pool_plugin_writes(events: &Events) -> Vec<StoreWrite> {
    let mut writes = Vec::new();
    for event in &events.pool_events {
        let Some(Typ::Plugin(plugin)) = &event.typ else {
            continue;
        };
        let ordinal = event.log_ordinal;
        let pool = hex::encode(&event.pool_address);
        writes.push(StoreWrite { ordinal, key: pool.clone(), value: hex::encode(&plugin.plugin) });
        writes.push(StoreWrite { ordinal, key: plugin_pool_key(&plugin.plugin), value: pool });
    }
    writes
}

/// Tracks what [`pool_plugin_writes`] lists.
#[substreams::handlers::store]
pub fn store_pool_plugins(events: Events, store: StoreSetString) {
    for write in pool_plugin_writes(&events) {
        store.set(write.ordinal, write.key, &write.value);
    }
}
