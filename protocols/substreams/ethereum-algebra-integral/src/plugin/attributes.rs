//! Pool attributes from a plugin's storage writes.

use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::{Attribute, ChangeType};

use crate::plugin::{
    modules::fields_at,
    storage::{word, StorageField},
};

/// What the pool already knows about the plugin whose writes are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emission {
    /// The pool has just named the plugin: every indexed field of a written slot is new to it,
    /// whatever the slot held before.
    Creation,
    /// The pool already carries the plugin's attributes: only fields whose bytes changed are
    /// emitted.
    Update,
}

/// The attributes one plugin's storage writes set on its pool.
///
/// `writes` are the writes to a single plugin's storage, from non-reverted calls and in ordinal
/// order. The caller selects them by the plugin's address: the slots of [`fields_at`] mean
/// nothing in another contract. A slot written several times counts once, from its first old
/// value to its last new one. Slots no module indexes are ignored.
///
/// A slot the plugin never wrote emits nothing. Fields sharing a slot arrive together at
/// creation, so a decoder that finds one of them finds all of them.
pub fn plugin_attributes(writes: &[StorageChange], emission: Emission) -> Vec<Attribute> {
    let mut attributes = Vec::new();
    for (index, write) in writes.iter().enumerate() {
        let earlier = &writes[..index];
        if earlier
            .iter()
            .any(|other| other.key == write.key)
        {
            continue;
        }
        let Ok(slot) = <[u8; 32]>::try_from(write.key.as_slice()) else {
            continue;
        };
        let last = writes[index..]
            .iter()
            .rfind(|other| other.key == write.key)
            .unwrap_or(write);
        let old = word(&write.old_value);
        let new = word(&last.new_value);
        for field in fields_at(&slot) {
            if let Some(attribute) = field_attribute(field, &old, &new, emission) {
                attributes.push(attribute);
            }
        }
    }
    attributes
}

fn field_attribute(
    field: &StorageField,
    old: &[u8; 32],
    new: &[u8; 32],
    emission: Emission,
) -> Option<Attribute> {
    let value = field.read(new);
    let change = match emission {
        Emission::Creation => ChangeType::Creation,
        Emission::Update => {
            if field.read(old) == value {
                return None;
            }
            ChangeType::Update
        }
    };
    Some(Attribute { name: field.name().to_string(), value: value.to_vec(), change: change.into() })
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;

    const FEE_CONFIG_SLOT: [u8; 32] =
        hex!("fbbf1a562c70d290e080160018965a1e5db682cf55e666eca8f391a4ceef9a00");
    const UNINDEXED_SLOT: [u8; 32] =
        hex!("4cc5c36a434034246042af122c958f40937cad4aaa0154d3b81e721d7b524c00");
    const BASE_FEE_500: [u8; 32] =
        hex!("000000000000000000000000000001f400010001000000000000000000000000");
    const BASE_FEE_800: [u8; 32] =
        hex!("0000000000000000000000000000032000010001000000000000000000000000");
    const BASE_FEE_800_ALPHA1_7: [u8; 32] =
        hex!("0000000000000000000000000000032000010001000000000000000000000007");

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
    fn creation_emits_every_field_of_a_written_slot() {
        let writes = [write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500)];

        let attributes = plugin_attributes(&writes, Emission::Creation);

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
    fn update_emits_only_the_fields_that_changed() {
        let writes = [write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800)];

        let attributes = plugin_attributes(&writes, Emission::Update);

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

        assert!(plugin_attributes(&writes, Emission::Update).is_empty());
    }

    #[test]
    fn several_writes_to_a_slot_count_from_first_old_to_last_new() {
        let writes = [
            write(&FEE_CONFIG_SLOT, &BASE_FEE_500, &BASE_FEE_800_ALPHA1_7),
            write(&UNINDEXED_SLOT, &[], &[1]),
            write(&FEE_CONFIG_SLOT, &BASE_FEE_800_ALPHA1_7, &BASE_FEE_800),
        ];

        let attributes = plugin_attributes(&writes, Emission::Update);

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

        assert!(plugin_attributes(&writes, Emission::Update).is_empty());
    }

    #[test]
    fn unindexed_and_malformed_slots_are_ignored() {
        let writes = [
            write(&UNINDEXED_SLOT, &[], &BASE_FEE_500),
            write(&FEE_CONFIG_SLOT[..31], &[], &BASE_FEE_500),
        ];

        assert!(plugin_attributes(&writes, Emission::Creation).is_empty());
    }

    #[test]
    fn values_with_leading_zeros_dropped_decode_the_same() {
        let writes = [write(&FEE_CONFIG_SLOT, &[], &BASE_FEE_500[13..])];

        let attributes = plugin_attributes(&writes, Emission::Creation);

        assert_eq!(attributes[2].value, hex!("01f4"));
    }

    #[test]
    fn no_writes_emit_nothing() {
        assert!(plugin_attributes(&[], Emission::Creation).is_empty());
    }
}
