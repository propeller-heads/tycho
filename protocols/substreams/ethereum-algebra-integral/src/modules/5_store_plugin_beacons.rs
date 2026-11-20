//! Which beacon each pool's plugin is bound to, and which pools each beacon serves.
//!
//! A plugin is a beacon proxy: it logs `BeaconUpgraded(beacon)` when it is constructed and again
//! if it is ever bound to another beacon. A beacon's later `Upgraded` log changes the
//! implementation of every pool whose plugin is bound to it, so that log has to be fanned out
//! to those pools.

use substreams::store::{
    Appender, StoreAppend, StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString,
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};

use crate::{abi::beacon::events::BeaconUpgraded, modules::store_pool_plugins::plugin_pool_key};

/// Store key of the beacon (hex) a plugin is bound to.
pub fn plugin_beacon_key(plugin: &[u8]) -> String {
    format!("plugin:{}", hex::encode(plugin))
}

/// Store key present for every beacon some pool's plugin has been bound to.
pub fn tracked_beacon_key(beacon: &[u8]) -> String {
    format!("beacon:{}", hex::encode(beacon))
}

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
    pool_plugins_store: &StoreGetString,
) -> Vec<BeaconBinding> {
    let mut bindings = Vec::new();
    for tx in block.transactions() {
        for (log, _) in tx.logs_with_calls() {
            // Match first: few logs are this event, and a store read costs more than a topic
            // comparison.
            let Some(event) = BeaconUpgraded::match_and_decode(log) else {
                continue;
            };
            let Some(pool) = pool_plugins_store.get_last(plugin_pool_key(&log.address)) else {
                continue;
            };
            if pool_plugins_store.get_last(&pool) != Some(hex::encode(&log.address)) {
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

/// Tracks the beacon (hex) each plugin is bound to, under [`plugin_beacon_key`], and marks every
/// such beacon under [`tracked_beacon_key`].
#[substreams::handlers::store]
pub fn store_plugin_beacons(
    block: eth::Block,
    pool_plugins_store: StoreGetString,
    store: StoreSetString,
) {
    for binding in beacon_bindings(&block, &pool_plugins_store) {
        store.set(
            binding.ordinal,
            plugin_beacon_key(&binding.plugin),
            &hex::encode(&binding.beacon),
        );
        store.set(binding.ordinal, tracked_beacon_key(&binding.beacon), &"1".to_string());
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
        ];

        assert_eq!(
            keys.iter()
                .collect::<HashSet<_>>()
                .len(),
            3
        );
    }
}
