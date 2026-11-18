//! Maps each hook storage slot of a Montra pool back to the pool and word it holds.
//!
//! A storage change names only its slot, and a slot is a keccak of the pool id, which cannot be
//! inverted; this store is how `map_montra_enriched_block_changes` tells which pool a hook write
//! belongs to. It holds six keys per pool (`PoolConfig` words 0 to 2, `PoolKey` words 0 to 2),
//! set when the pool is created and never changed.

use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString};
use tycho_substreams::prelude::*;

use crate::montra::pool_config::{pool_slots, slot_store_key, SlotOwner};

#[substreams::handlers::store]
pub fn store_montra_slots(pools_created: BlockEntityChanges, store: StoreSetIfNotExistsString) {
    for (key, value) in slot_entries(&pools_created) {
        store.set_if_not_exists(0, key, &value);
    }
}

/// The store entries of every pool `pools_created` creates.
fn slot_entries(pools_created: &BlockEntityChanges) -> Vec<(String, String)> {
    pools_created
        .changes
        .iter()
        .flat_map(|change| &change.component_changes)
        .flat_map(|component| {
            let pool_id = component
                .get_attribute_value("pool_id")
                .and_then(|value| <[u8; 32]>::try_from(value).ok())
                // `map_montra_pools_created` writes the 32-byte pool id of every component it
                // creates.
                .expect("map_montra_pools_created emits a 32-byte pool_id");
            pool_slots(&pool_id)
                .into_iter()
                .map(|(word, slot)| {
                    let owner = SlotOwner { component_id: component.id.clone(), word };
                    (slot_store_key(&slot), owner.to_store_value())
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_created_pool_maps_its_six_slots_to_its_words() {
        let id = "0x1f3bfa7c7d85bb77019e40018f2d179c04e413d7d853d9c77017a221ac4c2e6a";
        let pools_created = BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                component_changes: vec![ProtocolComponent::new(id)
                    .with_attributes(&[("pool_id", hex::decode(&id[2..]).unwrap())])],
                ..Default::default()
            }],
        };

        let entries = slot_entries(&pools_created);

        let config = "slot:0x306fdf70b142ad82d1dc56d41dbe6c099ce69473a61006b145ded747bd98c50";
        let key = "slot:0x518db8df6301ffbfe5428c38edb1859b34339c4608b623820c352f1b99dfbc0";
        let expected: Vec<(String, String)> = [
            (format!("{config}5"), 0),
            (format!("{config}6"), 1),
            (format!("{config}7"), 2),
            (format!("{key}6"), 4),
            (format!("{key}7"), 5),
            (format!("{key}8"), 6),
        ]
        .into_iter()
        .map(|(slot, index)| (slot, format!("{id}:{index}")))
        .collect();
        assert_eq!(entries, expected);
    }
}
