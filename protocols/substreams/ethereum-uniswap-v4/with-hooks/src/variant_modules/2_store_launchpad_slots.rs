//! Maps each hook storage slot of a launchpad pool back to the pool and word it holds.
//!
//! A storage change names only its contract and slot, and a slot is a keccak of the pool id, which
//! cannot be inverted; this store is how `map_launchpad_enriched_block_changes` tells which pool a
//! hook write belongs to. Each key names the pool's hook and the slot, and is set when the pool is
//! created and never changed. A Montra pool has six (`PoolConfig` words 0 to 2, `PoolKey` words 0
//! to 2), an Ascend pool five (`PoolData` words 2 and 3, `Bid` words 0 to 2). The layout comes
//! from the same parameters the creation and terminal modules read, so the three cannot disagree.

use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString};
use tycho_substreams::prelude::*;

use crate::launchpad::{
    params::Params,
    slots::{pool_slots, slot_store_key, SlotOwner},
};

#[substreams::handlers::store]
pub fn store_launchpad_slots(
    params: String,
    pools_created: BlockEntityChanges,
    store: StoreSetIfNotExistsString,
) {
    let params = Params::parse(&params)
        .unwrap_or_else(|e| panic!("store_launchpad_slots cannot read its params: {e}"));
    for (key, value) in slot_entries(&params, &pools_created) {
        store.set_if_not_exists(0, key, &value);
    }
}

/// The store entries of every pool `pools_created` creates, in the layout of the pool's hook.
///
/// Panics when a component lacks the 32-byte `pool_id` or 20-byte `hooks` that
/// `map_launchpad_pools_created` writes, or when its hook is in neither hook list: that module
/// creates pools of listed hooks only, so either means the manifest wired another module in.
fn slot_entries(params: &Params, pools_created: &BlockEntityChanges) -> Vec<(String, String)> {
    pools_created
        .changes
        .iter()
        .flat_map(|change| &change.component_changes)
        .flat_map(|component| {
            let pool_id = component
                .get_attribute_value("pool_id")
                .and_then(|value| <[u8; 32]>::try_from(value).ok())
                .expect("map_launchpad_pools_created emits a 32-byte pool_id");
            let hook = component
                .get_attribute_value("hooks")
                .and_then(|value| <[u8; 20]>::try_from(value).ok())
                .expect("map_launchpad_pools_created emits a 20-byte hooks");
            let layout = params.layout(&hook).unwrap_or_else(|| {
                panic!(
                    "pool {} has hook 0x{}, which no hook list names",
                    component.id,
                    hex::encode(hook)
                )
            });
            pool_slots(layout, &pool_id)
                .into_iter()
                .map(move |(word, slot)| {
                    let owner = SlotOwner { component_id: component.id.clone(), word };
                    (slot_store_key(&hook, &slot), owner.to_store_value())
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launchpad::test_fixtures::{params, ASCEND_HOOK, ASCEND_POOL, ASCEND_POOL_ID, HOOK};

    #[test]
    fn test_created_pool_maps_its_six_slots_to_its_words() {
        let id = "0x1f3bfa7c7d85bb77019e40018f2d179c04e413d7d853d9c77017a221ac4c2e6a";
        let pools_created = BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                component_changes: vec![ProtocolComponent::new(id).with_attributes(&[
                    ("pool_id", hex::decode(&id[2..]).unwrap()),
                    ("hooks", HOOK.to_vec()),
                ])],
                ..Default::default()
            }],
        };

        let entries = slot_entries(&params(), &pools_created);

        let hook = "0x6b1216a40b65c83e9c9e18a562c446697b8cf8ec";
        let config = format!(
            "slot:{hook}:0x306fdf70b142ad82d1dc56d41dbe6c099ce69473a61006b145ded747bd98c50"
        );
        let key = format!(
            "slot:{hook}:0x518db8df6301ffbfe5428c38edb1859b34339c4608b623820c352f1b99dfbc0"
        );
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

    /// An Ascend pool maps its five watched slots, `PoolData+2`, `PoolData+3` and `Bid+0..2`, to
    /// word indexes 7 to 11. The slots are `cast index bytes32 <id> 4` plus 2 and 3, and
    /// `cast index bytes32 <id> 2` plus 0 to 2, for pool 0xa138ea3b... (foundry's cast).
    #[test]
    fn test_created_ascend_pool_maps_its_five_slots_to_its_words() {
        let id = ASCEND_POOL;
        let pools_created = BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                component_changes: vec![ProtocolComponent::new(id).with_attributes(&[
                    ("pool_id", ASCEND_POOL_ID.to_vec()),
                    ("hooks", ASCEND_HOOK.to_vec()),
                ])],
                ..Default::default()
            }],
        };

        let entries = slot_entries(&params(), &pools_created);

        let hook = "0xe0def4221b9840a610d4e7d55812a79a6431a8ec";
        let pool_data = "0xeed4aedf482dc31cd5db10dafcce24e9cd678d08586286766e5957130525";
        let bid = "0x3fc5cc6e735d08aa6f82fe1e4f7818d4372f3a52e5f4ac7f60a89c2c4a4d4f";
        let expected: Vec<(String, String)> = [
            (format!("slot:{hook}:{pool_data}66e1"), 7),
            (format!("slot:{hook}:{pool_data}66e2"), 8),
            (format!("slot:{hook}:{bid}fd"), 9),
            (format!("slot:{hook}:{bid}fe"), 10),
            (format!("slot:{hook}:{bid}ff"), 11),
        ]
        .into_iter()
        .map(|(slot, index)| (slot, format!("{id}:{index}")))
        .collect();
        assert_eq!(entries, expected);
    }

    /// A pool whose hook is in neither list cannot come from `map_launchpad_pools_created`, so
    /// the store refuses it rather than guess a layout.
    #[test]
    #[should_panic(expected = "which no hook list names")]
    fn test_pool_of_an_unlisted_hook_panics() {
        let id = "0x1f3bfa7c7d85bb77019e40018f2d179c04e413d7d853d9c77017a221ac4c2e6a";
        let pools_created = BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                component_changes: vec![ProtocolComponent::new(id).with_attributes(&[
                    ("pool_id", hex::decode(&id[2..]).unwrap()),
                    ("hooks", [0x80; 20].to_vec()),
                ])],
                ..Default::default()
            }],
        };

        slot_entries(&params(), &pools_created);
    }
}
