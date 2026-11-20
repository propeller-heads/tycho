//! Maps each hook storage slot of a Montra or Ascend pool back to the pool and word it holds.
//!
//! A storage change names only its contract and slot, and a slot is a keccak of the pool id, which
//! cannot be inverted; this store is how `map_montra_enriched_block_changes` tells which pool a
//! hook write belongs to. It holds six keys per pool (`PoolConfig` words 0 to 2, `PoolKey` words 0
//! to 2), each keyed by the pool's hook and the slot, set when the pool is created and never
//! changed. An Ascend pool has five (`PoolData` words 2 and 3, `Bid` words 0 to 2): the hook's
//! membership in `ascend_hooks`, the module's only parameter, picks the layout; the creation module
//! creates pools of no other hooks than the Montra and Ascend ones.

use std::collections::BTreeSet;

use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString};
use tycho_substreams::prelude::*;

use crate::{
    address_list::Address,
    ascend,
    montra::{
        params::parse_ascend_hooks,
        pool_config::{pool_slots, slot_store_key, HookWord, SlotOwner},
    },
};

#[substreams::handlers::store]
pub fn store_montra_slots(
    params: String,
    pools_created: BlockEntityChanges,
    store: StoreSetIfNotExistsString,
) {
    let ascend_hooks = parse_ascend_hooks(&params)
        .unwrap_or_else(|e| panic!("store_montra_slots cannot read its params: {e}"));
    for (key, value) in slot_entries(&ascend_hooks, &pools_created) {
        store.set_if_not_exists(0, key, &value);
    }
}

/// The store entries of every pool `pools_created` creates; a pool of a hook in `ascend_hooks`
/// has the Ascend layout, any other the Montra one.
fn slot_entries(
    ascend_hooks: &BTreeSet<Address>,
    pools_created: &BlockEntityChanges,
) -> Vec<(String, String)> {
    pools_created
        .changes
        .iter()
        .flat_map(|change| &change.component_changes)
        .flat_map(|component| {
            // `map_montra_pools_created` writes the 32-byte pool id and the 20-byte hook of every
            // component it creates.
            let pool_id = component
                .get_attribute_value("pool_id")
                .and_then(|value| <[u8; 32]>::try_from(value).ok())
                .expect("map_montra_pools_created emits a 32-byte pool_id");
            let hook = component
                .get_attribute_value("hooks")
                .and_then(|value| Address::try_from(value).ok())
                .expect("map_montra_pools_created emits a 20-byte hooks");
            let slots = if ascend_hooks.contains(&hook) {
                ascend::pool_slots(&pool_id)
                    .into_iter()
                    .map(|(word, slot)| (HookWord::Ascend(word), slot))
                    .collect()
            } else {
                pool_slots(&pool_id)
            };
            slots
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
    use crate::montra::test_fixtures::{params, ASCEND_HOOK, HOOK};

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

        let entries = slot_entries(params().ascend_hooks(), &pools_created);

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
        let id = "0xa138ea3bc220ee819a81be441ad1fd95f84e5ff228a6c1d862376c66e60a01a5";
        let pools_created = BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                component_changes: vec![ProtocolComponent::new(id).with_attributes(&[
                    ("pool_id", hex::decode(&id[2..]).unwrap()),
                    ("hooks", ASCEND_HOOK.to_vec()),
                ])],
                ..Default::default()
            }],
        };

        let entries = slot_entries(params().ascend_hooks(), &pools_created);

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
}
