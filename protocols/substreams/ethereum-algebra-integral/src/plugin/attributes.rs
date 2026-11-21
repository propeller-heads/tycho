//! Pool attributes from a plugin's storage writes.

use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::{Attribute, ChangeType};

use crate::plugin::{
    modules::PluginModule,
    storage::{word, StorageField},
};

/// What one transaction did to one slot: its value before the first write and after the last.
struct SlotChange {
    slot: [u8; 32],
    old: [u8; 32],
    new: [u8; 32],
}

/// The slots `writes` touch, in the order they are first written. Keys that are not a full word
/// are skipped.
fn slot_changes(writes: &[StorageChange]) -> Vec<SlotChange> {
    let mut changes: Vec<SlotChange> = Vec::new();
    for write in writes {
        let Ok(slot) = <[u8; 32]>::try_from(write.key.as_slice()) else {
            continue;
        };
        let new = word(&write.new_value);
        match changes
            .iter_mut()
            .find(|change| change.slot == slot)
        {
            Some(change) => change.new = new,
            None => changes.push(SlotChange { slot, old: word(&write.old_value), new }),
        }
    }
    changes
}

fn attribute(field: &StorageField, word: &[u8; 32], change: ChangeType) -> Attribute {
    Attribute {
        name: field.name().to_string(),
        value: field.read(word).to_vec(),
        change: change.into(),
    }
}

/// The attributes a pool gains from a plugin created in the transaction that names it.
///
/// `writes` are the writes to that plugin's storage in the transaction, from non-reverted calls
/// and in ordinal order. Every field of `modules` is emitted, in table order: with the value the
/// plugin wrote, or zero for a slot it left untouched, which is what the storage of a contract
/// created in this transaction holds. A decoder therefore finds every field of the deployment's
/// modules on every pool, and a missing one means the package does not index it.
///
/// For a plugin created in an earlier transaction the zeros would be a guess; see
/// [`crate::plugin::changes::named_plugin_attributes`].
pub fn creation_attributes(writes: &[StorageChange], modules: &[&PluginModule]) -> Vec<Attribute> {
    const UNTOUCHED: [u8; 32] = [0; 32];
    let changes = slot_changes(writes);
    let mut attributes = Vec::new();
    for module in modules {
        for field in module.fields {
            let word = changes
                .iter()
                .find(|change| &change.slot == field.slot())
                .map_or(&UNTOUCHED, |change| &change.new);
            attributes.push(attribute(field, word, ChangeType::Creation));
        }
    }
    attributes
}

/// The attributes one transaction's writes change on a pool that already carries its plugin's.
///
/// `writes` are as for [`creation_attributes`]. Only fields of `modules` whose bytes changed are
/// emitted, in write order. A slot written several times counts once, from its first old value
/// to its last new one. Slots none of `modules` lists are ignored.
pub fn update_attributes(writes: &[StorageChange], modules: &[&PluginModule]) -> Vec<Attribute> {
    let mut attributes = Vec::new();
    for change in slot_changes(writes) {
        for module in modules {
            for field in module.fields {
                let changed = field.slot() == &change.slot &&
                    field.read(&change.old) != field.read(&change.new);
                if changed {
                    attributes.push(attribute(field, &change.new, ChangeType::Update));
                }
            }
        }
    }
    attributes
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;
    use crate::plugin::modules::algebra::dynamic_fee;

    const FEE_CONFIG_SLOT: [u8; 32] =
        hex!("fbbf1a562c70d290e080160018965a1e5db682cf55e666eca8f391a4ceef9a00");
    const UNINDEXED_SLOT: [u8; 32] =
        hex!("4cc5c36a434034246042af122c958f40937cad4aaa0154d3b81e721d7b524c00");
    const OTHER_SLOT: [u8; 32] = [0x11; 32];
    const BASE_FEE_500: [u8; 32] =
        hex!("000000000000000000000000000001f400010001000000000000000000000000");
    const BASE_FEE_800: [u8; 32] =
        hex!("0000000000000000000000000000032000010001000000000000000000000000");
    const BASE_FEE_800_ALPHA1_7: [u8; 32] =
        hex!("0000000000000000000000000000032000010001000000000000000000000007");

    /// A module of another slot, with a field as wide as an address.
    const OTHER: PluginModule = PluginModule {
        name: "other",
        fields: &[
            StorageField::new("other/flag", OTHER_SLOT, 20, 1),
            StorageField::new("other/owner", OTHER_SLOT, 0, 20),
        ],
    };
    const DYNAMIC_FEE: &[&PluginModule] = &[&dynamic_fee::MODULE];

    fn write(slot: &[u8], old: &[u8], new: &[u8]) -> StorageChange {
        StorageChange {
            key: slot.to_vec(),
            old_value: old.to_vec(),
            new_value: new.to_vec(),
            ..Default::default()
        }
    }

    fn decoded(attributes: &[Attribute]) -> Vec<(&str, &[u8], ChangeType)> {
        attributes
            .iter()
            .map(|a| (a.name.as_str(), a.value.as_slice(), a.change()))
            .collect()
    }

    #[test]
    fn creation_emits_written_values() {
        let writes = [write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500)];

        let attributes = creation_attributes(&writes, DYNAMIC_FEE);

        assert_eq!(
            decoded(&attributes),
            [
                ("dynamic_fee/alpha1", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/alpha2", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/base_fee", hex!("01f4").as_slice(), ChangeType::Creation),
            ]
        );
    }

    #[test]
    fn creation_emits_zero_at_each_fields_width_for_an_untouched_slot() {
        let writes = [write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500)];

        let attributes = creation_attributes(&writes, &[&OTHER, &dynamic_fee::MODULE]);

        assert_eq!(
            decoded(&attributes),
            [
                ("other/flag", [0u8; 1].as_slice(), ChangeType::Creation),
                ("other/owner", [0u8; 20].as_slice(), ChangeType::Creation),
                ("dynamic_fee/alpha1", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/alpha2", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/base_fee", hex!("01f4").as_slice(), ChangeType::Creation),
            ]
        );
    }

    #[test]
    fn creation_without_writes_emits_every_field_as_zero() {
        let attributes = creation_attributes(&[], DYNAMIC_FEE);

        assert_eq!(
            decoded(&attributes),
            [
                ("dynamic_fee/alpha1", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/alpha2", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/base_fee", hex!("0000").as_slice(), ChangeType::Creation),
            ]
        );
    }

    #[test]
    fn creation_takes_the_last_value_written_to_a_slot() {
        let writes = [
            write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500),
            write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800),
        ];

        let attributes = creation_attributes(&writes, DYNAMIC_FEE);

        assert_eq!(attributes[2].value, hex!("0320"));
    }

    #[test]
    fn creation_leaves_out_modules_the_deployment_does_not_list() {
        let writes = [write(&OTHER_SLOT, &[], &[1])];

        let attributes = creation_attributes(&writes, DYNAMIC_FEE);

        assert!(attributes
            .iter()
            .all(|attribute| attribute
                .name
                .starts_with("dynamic_fee/")));
    }

    #[test]
    fn creation_with_no_modules_emits_nothing() {
        let writes = [write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500)];

        assert!(creation_attributes(&writes, &[]).is_empty());
    }

    #[test]
    fn values_with_leading_zeros_dropped_decode_the_same() {
        let writes = [write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500[13..])];

        let attributes = creation_attributes(&writes, DYNAMIC_FEE);

        assert_eq!(attributes[2].value, hex!("01f4"));
    }

    #[test]
    fn update_emits_only_the_fields_that_changed() {
        let writes = [write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800)];

        let attributes = update_attributes(&writes, DYNAMIC_FEE);

        assert_eq!(
            decoded(&attributes),
            [("dynamic_fee/base_fee", hex!("0320").as_slice(), ChangeType::Update)]
        );
    }

    #[test]
    fn update_that_changes_no_indexed_field_emits_nothing() {
        // Only gamma1 differs, which is not indexed.
        let gamma1_2 = hex!("000000000000000000000000000001f400010002000000000000000000000000");
        let writes = [write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &gamma1_2)];

        assert!(update_attributes(&writes, DYNAMIC_FEE).is_empty());
    }

    #[test]
    fn several_writes_to_a_slot_count_from_first_old_to_last_new() {
        let writes = [
            write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800_ALPHA1_7),
            write(&UNINDEXED_SLOT, &[], &[1]),
            write(&FEE_CONFIG_SLOT, &BASE_FEE_800_ALPHA1_7, &BASE_FEE_800),
        ];

        let attributes = update_attributes(&writes, DYNAMIC_FEE);

        assert_eq!(
            decoded(&attributes),
            [("dynamic_fee/base_fee", hex!("0320").as_slice(), ChangeType::Update)]
        );
    }

    #[test]
    fn write_restored_within_the_transaction_emits_nothing() {
        let writes = [
            write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800),
            write(&FEE_CONFIG_SLOT, &BASE_FEE_800, &BASE_FEE_500),
        ];

        assert!(update_attributes(&writes, DYNAMIC_FEE).is_empty());
    }

    #[test]
    fn update_ignores_unindexed_and_malformed_slots() {
        let writes = [
            write(&UNINDEXED_SLOT, &[], &BASE_FEE_500),
            write(&FEE_CONFIG_SLOT[..31], &[], &BASE_FEE_500),
        ];

        assert!(update_attributes(&writes, DYNAMIC_FEE).is_empty());
    }

    #[test]
    fn update_ignores_modules_the_deployment_does_not_list() {
        let writes = [write(&OTHER_SLOT, &[], &[1])];

        assert!(update_attributes(&writes, DYNAMIC_FEE).is_empty());
    }

    #[test]
    fn update_emits_in_write_order() {
        let flag_set = hex!("0000000000000000000000010000000000000000000000000000000000000000");
        let writes = [
            write(&OTHER_SLOT, &[], &flag_set),
            write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800),
        ];

        let attributes = update_attributes(&writes, &[&dynamic_fee::MODULE, &OTHER]);

        assert_eq!(
            decoded(&attributes),
            [
                ("other/flag", [1u8].as_slice(), ChangeType::Update),
                ("dynamic_fee/base_fee", hex!("0320").as_slice(), ChangeType::Update),
            ]
        );
    }
}
