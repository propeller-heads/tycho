//! Storage layouts of the plugin modules this package can index.
//!
//! One directory per vendor ([`algebra`] holds Algebra's own modules; a DEX with modules of its
//! own adds a directory named after it), one file per module. A module file defines the module's
//! slots as constants and exports a `MODULE`; adding that to [`MODULES`] is what makes the
//! package emit it. Nothing else in the package names a module.
//!
//! The table is not keyed by DEX or by plugin implementation: it is applied to every plugin. A
//! module's slots are hashes of its namespace, so a plugin without the module never writes them.

use crate::plugin::storage::StorageField;

pub mod algebra;
pub mod nami;

/// One plugin module's indexed storage.
#[derive(Debug)]
pub struct PluginModule {
    /// Prefix of every attribute the module emits: `<name>/<field>`.
    pub name: &'static str,
    /// Only the fields the simulation reads.
    pub fields: &'static [StorageField],
}

/// Every module the package indexes.
pub const MODULES: &[&PluginModule] = &[
    &algebra::dynamic_fee::MODULE,
    &algebra::farming::MODULE,
    &nami::anti_sniper::MODULE,
    &nami::fee_override::MODULE,
    &nami::mevx::MODULE,
];

/// The indexed fields stored at `slot`, across all modules.
pub(crate) fn fields_at(slot: &[u8; 32]) -> impl Iterator<Item = &'static StorageField> + '_ {
    MODULES
        .iter()
        .flat_map(|module| module.fields.iter())
        .filter(move |field| field.slot() == slot)
}

/// Whether some module indexes a field stored at `slot`.
pub fn is_module_slot(slot: &[u8]) -> bool {
    <&[u8; 32]>::try_from(slot).is_ok_and(|slot| fields_at(slot).next().is_some())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[test]
    fn module_names_are_unique() {
        let mut names = HashSet::new();
        for module in MODULES {
            assert!(names.insert(module.name), "module {} is listed twice", module.name);
        }
    }

    #[test]
    fn attribute_names_carry_their_modules_prefix_and_are_unique() {
        let mut names = HashSet::new();
        for module in MODULES {
            let prefix = format!("{}/", module.name);
            for field in module.fields {
                assert!(
                    field.name().starts_with(&prefix),
                    "{} is not under {prefix}",
                    field.name()
                );
                assert!(names.insert(field.name()), "{} is emitted twice", field.name());
            }
        }
    }

    #[test]
    fn no_slot_belongs_to_two_modules() {
        let mut owners: HashMap<[u8; 32], &str> = HashMap::new();
        for module in MODULES {
            for field in module.fields {
                let owner = owners
                    .entry(*field.slot())
                    .or_insert(module.name);
                assert_eq!(*owner, module.name, "{} shares a slot with {owner}", field.name());
            }
        }
    }

    #[test]
    fn fields_in_one_slot_do_not_overlap() {
        let mut used: HashMap<[u8; 32], [Option<&str>; 32]> = HashMap::new();
        for module in MODULES {
            for field in module.fields {
                let bytes = used
                    .entry(*field.slot())
                    .or_insert([None; 32]);
                for byte in &mut bytes[field.byte_range()] {
                    if let Some(other) = byte {
                        panic!("{} overlaps {other}", field.name());
                    }
                    *byte = Some(field.name());
                }
            }
        }
    }
}
