//! Which beacon each pool's plugin is bound to, and which pools each beacon serves.
//!
//! A plugin is a beacon proxy: it logs `BeaconUpgraded(beacon)` when it is constructed and again
//! if it is ever bound to another beacon. A beacon's later `Upgraded` log changes the
//! implementation of every pool whose plugin is bound to it, so that log has to be fanned out
//! to those pools.
//!
//! The same store records which plugin attributes each pool carries, so that a later change
//! deletes only attributes that exist: see [`followed_plugin_key`].

use substreams::store::{
    Appender, StoreAppend, StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString,
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};

use crate::{
    abi::beacon::events::BeaconUpgraded,
    modules::store_pool_plugins::{
        decode_plugin, plugin_at, plugin_pool_key, StoreWrite, StringStore,
    },
    pb::algebra::integral::v1::{events::pool_event::Typ, Events},
    plugin::{
        beacon::plugin_beacon,
        changes::{named_plugin_attributes, PLUGIN_IMPL},
    },
};

/// Store key of the beacon (hex) a plugin is bound to.
pub fn plugin_beacon_key(plugin: &[u8]) -> String {
    format!("plugin:{}", hex::encode(plugin))
}

/// Store key present for every beacon some pool's plugin has been bound to.
pub fn tracked_beacon_key(beacon: &[u8]) -> String {
    format!("beacon:{}", hex::encode(beacon))
}

/// Store key of the plugin (hex) whose attributes a pool carries: the plugin the pool names,
/// when the transaction that named it also created it. Empty when the pool names no plugin, or
/// one created earlier, whose configuration is not known and whose pool is paused.
pub fn followed_plugin_key(pool: &[u8]) -> String {
    format!("followed:{}", hex::encode(pool))
}

/// Store key saying whether the pool that carries a plugin's attributes has its `plugin_impl`:
/// `1` when the implementation the plugin runs was last seen in a trace, empty otherwise.
pub fn known_implementation_key(plugin: &[u8]) -> String {
    format!("impl:{}", hex::encode(plugin))
}

/// The plugin whose attributes `pool` carries after every write of the block up to `ordinal`.
pub fn followed_plugin_at(
    plugin_beacons: &impl StringStore,
    ordinal: u64,
    pool: &[u8],
) -> Option<Vec<u8>> {
    decode_plugin(plugin_beacons.value_at(ordinal, &followed_plugin_key(pool)))
}

/// Whether the pool carrying `plugin`'s attributes has a `plugin_impl` after every write of the
/// block up to `ordinal`.
pub fn implementation_known_at(
    plugin_beacons: &impl StringStore,
    ordinal: u64,
    plugin: &[u8],
) -> bool {
    plugin_beacons
        .value_at(ordinal, &known_implementation_key(plugin))
        .is_some_and(|known| known == KNOWN)
}

const KNOWN: &str = "1";

/// How many keys a beacon's pools are spread over. A store value is bounded, and one beacon can
/// serve every pool of a DEX.
const POOL_SHARDS: u16 = 256;

fn beacon_pools_shard_key(beacon: &[u8], shard: u8) -> String {
    format!("pools:{}:{shard:02x}", hex::encode(beacon))
}

/// Store key of the list a pool is appended to when its plugin is bound to `beacon`. The shard
/// is the pool address's first byte.
fn beacon_pools_key(beacon: &[u8], pool: &[u8]) -> String {
    beacon_pools_shard_key(beacon, pool.first().copied().unwrap_or(0))
}

/// Every key the pools of `beacon` may be listed under.
pub fn beacon_pools_keys(beacon: &[u8]) -> impl Iterator<Item = String> + '_ {
    (0..POOL_SHARDS)
        .map(move |shard| beacon_pools_shard_key(beacon, u8::try_from(shard).unwrap_or(u8::MAX)))
}

/// A pool's plugin logging the beacon it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconBinding {
    pub pool: Vec<u8>,
    pub plugin: Vec<u8>,
    pub beacon: Vec<u8>,
    /// Ordinal of the `BeaconUpgraded` log.
    pub ordinal: u64,
    /// Index of the transaction that logged it.
    pub tx_index: u32,
}

/// The `BeaconUpgraded` logs of the block that come from a pool's current plugin.
///
/// A plugin resolves to its pool through the store's value at the end of the block: the proxy
/// logs its beacon in its constructor, before the pool's `Plugin` event names it. A proxy that
/// no pool names by the end of the block is not a plugin this package follows.
pub fn beacon_bindings(
    block: &eth::Block,
    pool_plugins_store: &impl StringStore,
) -> Vec<BeaconBinding> {
    let mut bindings = Vec::new();
    for tx in block.transactions() {
        for (log, _) in tx.logs_with_calls() {
            // Match first: few logs are this event, and a store read costs more than a topic
            // comparison.
            let Some(event) = BeaconUpgraded::match_and_decode(log) else {
                continue;
            };
            let Some(pool) = pool_plugins_store.value_at(u64::MAX, &plugin_pool_key(&log.address))
            else {
                continue;
            };
            if pool_plugins_store.value_at(u64::MAX, &pool) != Some(hex::encode(&log.address)) {
                continue;
            }
            let Ok(pool) = hex::decode(pool) else {
                continue;
            };
            bindings.push(BeaconBinding {
                pool,
                plugin: log.address.clone(),
                beacon: event.beacon,
                ordinal: log.ordinal,
                tx_index: tx.index,
            });
        }
    }
    bindings
}

/// What the block writes to `store_plugin_beacons`, in ordinal order:
///
/// - the beacon (hex) each plugin is bound to, under [`plugin_beacon_key`], and a marker for every
///   such beacon under [`tracked_beacon_key`];
/// - on each `Plugin` event that changes a pool's plugin, the plugin whose attributes the pool then
///   carries, under [`followed_plugin_key`];
/// - whether that pool has a `plugin_impl`, under [`known_implementation_key`], when a plugin is
///   named and each time it logs its beacon.
///
/// `map_protocol_changes` emits the attributes and reads these keys to tell which exist, so
/// both use `named_plugin_attributes` and `plugin_beacon` on the same transactions.
pub fn plugin_beacon_writes(
    block: &eth::Block,
    events: &Events,
    pool_plugins_store: &impl StringStore,
) -> Vec<StoreWrite> {
    let trace_of = |index: u32| {
        block
            .transaction_traces
            .iter()
            .find(|trace| trace.index == index)
    };
    let known = |known: bool| if known { KNOWN.to_string() } else { String::new() };
    let mut writes = Vec::new();

    for binding in beacon_bindings(block, pool_plugins_store) {
        let ordinal = binding.ordinal;
        writes.push(StoreWrite {
            ordinal,
            key: plugin_beacon_key(&binding.plugin),
            value: hex::encode(&binding.beacon),
        });
        writes.push(StoreWrite {
            ordinal,
            key: tracked_beacon_key(&binding.beacon),
            value: KNOWN.to_string(),
        });
        let implementation =
            trace_of(binding.tx_index).and_then(|trace| plugin_beacon(trace, &binding.plugin));
        writes.push(StoreWrite {
            ordinal,
            key: known_implementation_key(&binding.plugin),
            value: known(implementation.is_some()),
        });
    }

    for event in &events.pool_events {
        let Some(Typ::Plugin(named)) = &event.typ else {
            continue;
        };
        let ordinal = event.log_ordinal;
        let pool = &event.pool_address;
        let plugin = decode_plugin(Some(hex::encode(&named.plugin)));
        // Naming the plugin the pool already names changes nothing it carries.
        if plugin == plugin_at(pool_plugins_store, ordinal.saturating_sub(1), pool) {
            continue;
        }
        let attributes = plugin.as_ref().and_then(|plugin| {
            let trace = trace_of(
                event
                    .transaction
                    .as_ref()?
                    .index
                    .try_into()
                    .ok()?,
            )?;
            named_plugin_attributes(trace, plugin, &[])
        });
        let followed = match (&plugin, attributes) {
            (Some(plugin), Some(attributes)) => {
                let has_implementation = attributes
                    .iter()
                    .any(|attribute| attribute.name == PLUGIN_IMPL);
                writes.push(StoreWrite {
                    ordinal,
                    key: known_implementation_key(plugin),
                    value: known(has_implementation),
                });
                hex::encode(plugin)
            }
            (Some(_), None) | (None, _) => String::new(),
        };
        writes.push(StoreWrite { ordinal, key: followed_plugin_key(pool), value: followed });
    }

    writes.sort_by_key(|write| write.ordinal);
    writes
}

/// Tracks what [`plugin_beacon_writes`] lists.
#[substreams::handlers::store]
pub fn store_plugin_beacons(
    block: eth::Block,
    events: Events,
    pool_plugins_store: StoreGetString,
    store: StoreSetString,
) {
    for write in plugin_beacon_writes(&block, &events, &pool_plugins_store) {
        store.set(write.ordinal, write.key, &write.value);
    }
}

/// Lists the pools (hex) whose plugin has been bound to each beacon, under the keys of
/// [`beacon_pools_keys`]. A pool stays listed after its plugin moves to another beacon, so a
/// reader checks the plugin's current beacon in [`store_plugin_beacons`].
#[substreams::handlers::store]
pub fn store_beacon_pools(
    block: eth::Block,
    pool_plugins_store: StoreGetString,
    store: StoreAppend<String>,
) {
    for binding in beacon_bindings(&block, &pool_plugins_store) {
        store.append(
            binding.ordinal,
            beacon_pools_key(&binding.beacon, &binding.pool),
            hex::encode(&binding.pool),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const BEACON: [u8; 20] = [0x44; 20];

    #[test]
    fn pool_is_listed_under_one_of_its_beacons_keys() {
        let keys: HashSet<String> = beacon_pools_keys(&BEACON).collect();

        assert_eq!(keys.len(), 256);
        for first_byte in [0x00, 0x0e, 0xff] {
            let mut pool = [0x11; 20];
            pool[0] = first_byte;
            assert!(keys.contains(&beacon_pools_key(&BEACON, &pool)), "{first_byte:#04x}");
        }
    }

    #[test]
    fn keys_of_one_address_do_not_collide() {
        let keys = [
            plugin_beacon_key(&BEACON),
            tracked_beacon_key(&BEACON),
            beacon_pools_key(&BEACON, &BEACON),
            followed_plugin_key(&BEACON),
            known_implementation_key(&BEACON),
        ];

        assert_eq!(
            keys.iter()
                .collect::<HashSet<_>>()
                .len(),
            5
        );
    }
}
