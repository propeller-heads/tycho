//! A pool's plugin attributes from one transaction.
//!
//! The module that builds a pool's changes calls [`named_plugin_attributes`] in the transaction
//! whose `Plugin` event names the pool's plugin, and [`updated_plugin_attributes`] in every later
//! transaction, for the plugin the pool store resolves the pool to.
//!
//! Deployment assumption: a plugin's initial configuration is written in the transaction that
//! names it to its pool, as the plugin factory does when it creates both. A pool that is pointed
//! at a plugin configured in an earlier transaction gets only what that plugin writes from then
//! on; the caller pauses such a pool, which it recognises by the plugin not being created in the
//! naming transaction.

use substreams_ethereum::pb::eth::v2::{StorageChange, TransactionTrace};
use tycho_substreams::prelude::{Attribute, ChangeType};

use crate::plugin::{
    attributes::{plugin_attributes, Emission},
    beacon::plugin_beacon,
};

/// Attribute holding the implementation address a pool's plugin runs.
pub const PLUGIN_IMPL: &str = "plugin_impl";

/// `plugin`'s storage writes in `trx`, outside reverted calls, in write order.
fn plugin_writes(trx: &TransactionTrace, plugin: &[u8]) -> Vec<StorageChange> {
    let mut writes = Vec::new();
    for call in &trx.calls {
        if call.state_reverted {
            continue;
        }
        for change in &call.storage_changes {
            if change.address == plugin {
                writes.push(change.clone());
            }
        }
    }
    writes.sort_by_key(|change| change.ordinal);
    writes
}

/// The `plugin_impl` attribute for `implementation`.
pub fn implementation_attribute(implementation: &[u8], change: ChangeType) -> Attribute {
    Attribute {
        name: PLUGIN_IMPL.to_string(),
        value: implementation.to_vec(),
        change: change.into(),
    }
}

/// The attributes a pool gains in the transaction that names `plugin` as its plugin: the
/// implementation the plugin runs, when it is a beacon proxy constructed in `trx`, and the
/// configuration the plugin's modules wrote in `trx`.
pub fn named_plugin_attributes(trx: &TransactionTrace, plugin: &[u8]) -> Vec<Attribute> {
    let mut attributes = Vec::new();
    if let Some(bound) = plugin_beacon(trx, plugin) {
        attributes.push(implementation_attribute(&bound.implementation, ChangeType::Creation));
    }
    attributes.extend(plugin_attributes(&plugin_writes(trx, plugin), Emission::Creation));
    attributes
}

/// The module attributes `plugin`'s writes in `trx` change on the pool that already runs it.
pub fn updated_plugin_attributes(trx: &TransactionTrace, plugin: &[u8]) -> Vec<Attribute> {
    plugin_attributes(&plugin_writes(trx, plugin), Emission::Update)
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use substreams_ethereum::pb::eth::v2::{Call, Log};

    use super::*;

    const PLUGIN: [u8; 20] = hex!("2f643a734d8e090b82762bd982f1428ea482752e");
    const OTHER_PLUGIN: [u8; 20] = hex!("b0918bddea10fcc127bbe816725b2be536cebb8a");
    const BEACON: [u8; 20] = hex!("44ce18e6e618a35f05ddd384571fc32b3fa0b879");
    const IMPLEMENTATION: [u8; 20] = hex!("6580d2f0bab746ae2e655f92e2207f7eb0a13788");
    const FEE_CONFIG_SLOT: [u8; 32] =
        hex!("fbbf1a562c70d290e080160018965a1e5db682cf55e666eca8f391a4ceef9a00");
    const BASE_FEE_500: [u8; 32] =
        hex!("000000000000000000000000000001f400010001000000000000000000000000");
    const BASE_FEE_800: [u8; 32] =
        hex!("0000000000000000000000000000032000010001000000000000000000000000");

    fn word(address: &[u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(address);
        word
    }

    fn fee_config_write(plugin: &[u8; 20], old: &[u8], new: &[u8], ordinal: u64) -> StorageChange {
        StorageChange {
            address: plugin.to_vec(),
            key: FEE_CONFIG_SLOT.to_vec(),
            old_value: old.to_vec(),
            new_value: new.to_vec(),
            ordinal,
        }
    }

    /// The plugin's constructor, its read of the beacon and its initializer.
    fn creation_trx() -> TransactionTrace {
        TransactionTrace {
            calls: vec![
                Call {
                    address: PLUGIN.to_vec(),
                    logs: vec![Log {
                        address: PLUGIN.to_vec(),
                        topics: vec![
                            hex!(
                                "1cf3b03a6cf19fa2baba4df148e9dcabedea7f8a5c07840e207e5c089be95d3e"
                            )
                            .to_vec(),
                            word(&BEACON),
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Call {
                    caller: PLUGIN.to_vec(),
                    address: BEACON.to_vec(),
                    input: hex!("5c60da1b").to_vec(),
                    return_data: word(&IMPLEMENTATION),
                    ..Default::default()
                },
                Call {
                    address: PLUGIN.to_vec(),
                    storage_changes: vec![fee_config_write(&PLUGIN, &[], &BASE_FEE_500, 7)],
                    ..Default::default()
                },
            ],
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
    fn naming_emits_the_implementation_and_the_initial_configuration() {
        let attributes = named_plugin_attributes(&creation_trx(), &PLUGIN);

        assert_eq!(
            decoded(&attributes),
            [
                ("plugin_impl", IMPLEMENTATION.as_slice(), ChangeType::Creation),
                ("dynamic_fee/alpha1", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/alpha2", hex!("0000").as_slice(), ChangeType::Creation),
                ("dynamic_fee/base_fee", hex!("01f4").as_slice(), ChangeType::Creation),
            ]
        );
    }

    #[test]
    fn naming_a_plugin_created_elsewhere_emits_nothing() {
        assert!(named_plugin_attributes(&creation_trx(), &OTHER_PLUGIN).is_empty());
    }

    #[test]
    fn naming_a_plugin_that_is_not_a_beacon_proxy_emits_only_its_configuration() {
        let mut trx = creation_trx();
        trx.calls.truncate(3);
        trx.calls.remove(0);

        let attributes = named_plugin_attributes(&trx, &PLUGIN);

        assert_eq!(attributes.len(), 3);
        assert!(attributes
            .iter()
            .all(|attribute| attribute.name != PLUGIN_IMPL));
    }

    #[test]
    fn update_emits_changed_fields_of_this_plugin_only() {
        let trx = TransactionTrace {
            calls: vec![
                Call {
                    storage_changes: vec![fee_config_write(
                        &OTHER_PLUGIN,
                        &BASE_FEE_500,
                        &hex!("00"),
                        1,
                    )],
                    ..Default::default()
                },
                Call {
                    storage_changes: vec![fee_config_write(
                        &PLUGIN,
                        &BASE_FEE_500,
                        &BASE_FEE_800,
                        2,
                    )],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let attributes = updated_plugin_attributes(&trx, &PLUGIN);

        assert_eq!(
            decoded(&attributes),
            [("dynamic_fee/base_fee", hex!("0320").as_slice(), ChangeType::Update)]
        );
    }

    #[test]
    fn writes_in_reverted_calls_are_ignored() {
        let trx = TransactionTrace {
            calls: vec![Call {
                state_reverted: true,
                storage_changes: vec![fee_config_write(&PLUGIN, &BASE_FEE_500, &BASE_FEE_800, 2)],
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(updated_plugin_attributes(&trx, &PLUGIN).is_empty());
    }

    #[test]
    fn writes_across_calls_are_taken_in_ordinal_order() {
        // The later call frame is listed first; ordinals say 500 → 800 → 500.
        let trx = TransactionTrace {
            calls: vec![
                Call {
                    storage_changes: vec![fee_config_write(
                        &PLUGIN,
                        &BASE_FEE_800,
                        &BASE_FEE_500,
                        9,
                    )],
                    ..Default::default()
                },
                Call {
                    storage_changes: vec![fee_config_write(
                        &PLUGIN,
                        &BASE_FEE_500,
                        &BASE_FEE_800,
                        3,
                    )],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert!(updated_plugin_attributes(&trx, &PLUGIN).is_empty());
    }
}
