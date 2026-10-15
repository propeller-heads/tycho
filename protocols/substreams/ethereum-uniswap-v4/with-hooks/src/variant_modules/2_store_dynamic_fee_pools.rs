use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString};
use tycho_substreams::prelude::*;

use crate::dynamic_lp_fee::tracking_key;

/// Tracks the `slot0` slot of every dynamic-fee pool, keyed for [`crate::dynamic_lp_fee`], with
/// the pool's component id as the value.
#[substreams::handlers::store]
pub fn store_dynamic_fee_pools(
    pools_created: BlockEntityChanges,
    store: StoreSetIfNotExistsString,
) {
    for tx_changes in pools_created.changes {
        for component in tx_changes.component_changes {
            let attribute = |name: &str| {
                component
                    .static_att
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| a.value.as_slice())
            };
            let (Some(pool_id), Some(key_lp_fee)) = (attribute("pool_id"), attribute("key_lp_fee"))
            else {
                continue
            };
            if let Some(key) = tracking_key(pool_id, key_lp_fee) {
                store.set_if_not_exists(0, &key, &component.id);
            }
        }
    }
}
