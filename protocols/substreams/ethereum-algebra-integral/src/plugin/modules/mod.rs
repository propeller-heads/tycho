//! Storage layouts of the plugin modules this package can index.
//!
//! One directory per vendor ([`algebra`] holds Algebra's own modules; a DEX with modules of its
//! own adds a directory named after it), one file per module. A module file defines the module's
//! slots as constants and exports a `MODULE`; adding that to [`MODULES`] is what makes the
//! package emit it. Nothing else in the package names a module.
//!
//! The table is not keyed by DEX or by plugin implementation. A manifest names the modules its
//! deployment's plugins are built from in its `modules` param ([`modules_named`]), and those are
//! applied to every plugin of the deployment: a module's slots are hashes of its namespace, so a
//! plugin without the module never writes them and carries its fields as zero.

use std::fmt;

use crate::plugin::storage::StorageField;

pub mod algebra;

/// One plugin module's indexed storage.
#[derive(Debug)]
pub struct PluginModule {
    /// Prefix of every attribute the module emits: `<name>/<field>`.
    pub name: &'static str,
    /// Only the fields the simulation reads.
    pub fields: &'static [StorageField],
}

/// Every module the package indexes.
pub const MODULES: &[&PluginModule] = &[&algebra::dynamic_fee::MODULE];

/// A manifest's `modules` param that cannot be resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum ModuleListError {
    /// The package has no module of this name.
    Unknown(String),
    /// The name is listed twice, which would emit the module's attributes twice.
    Duplicate(String),
}

impl fmt::Display for ModuleListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => {
                let known: Vec<&str> = MODULES
                    .iter()
                    .map(|module| module.name)
                    .collect();
                write!(
                    f,
                    "the modules param names plugin module `{name}`, which this package does not \
                     index; it indexes {}",
                    known.join(", ")
                )
            }
            Self::Duplicate(name) => {
                write!(f, "the modules param names plugin module `{name}` twice")
            }
        }
    }
}

impl std::error::Error for ModuleListError {}

/// The modules a manifest's `modules` param names: module names separated by commas, in the
/// order their attributes are emitted. An empty param names none.
///
/// A manifest that names a module the package lacks must not run: every pool would be created
/// without that module's attributes.
pub fn modules_named(names: &str) -> Result<Vec<&'static PluginModule>, ModuleListError> {
    let mut modules: Vec<&'static PluginModule> = Vec::new();
    if names.is_empty() {
        return Ok(modules);
    }
    for name in names.split(',') {
        let Some(module) = MODULES
            .iter()
            .find(|module| module.name == name)
        else {
            return Err(ModuleListError::Unknown(name.to_string()));
        };
        if modules
            .iter()
            .any(|listed| listed.name == name)
        {
            return Err(ModuleListError::Duplicate(name.to_string()));
        }
        modules.push(module);
    }
    Ok(modules)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use rstest::rstest;

    use super::*;

    #[test]
    fn modules_named_resolves_names_in_the_order_given() {
        let modules = modules_named("dynamic_fee").expect("dynamic_fee is indexed");

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "dynamic_fee");
    }

    #[test]
    fn modules_named_takes_an_empty_param_as_no_modules() {
        assert!(modules_named("")
            .expect("an empty param is valid")
            .is_empty());
    }

    #[rstest]
    #[case::unknown("dynamic_fee,nope", ModuleListError::Unknown("nope".to_string()))]
    #[case::padded("dynamic_fee, dynamic_fee", ModuleListError::Unknown(" dynamic_fee".to_string()))]
    #[case::trailing_comma("dynamic_fee,", ModuleListError::Unknown(String::new()))]
    #[case::duplicate("dynamic_fee,dynamic_fee", ModuleListError::Duplicate("dynamic_fee".to_string()))]
    fn modules_named_rejects(#[case] names: &str, #[case] expected: ModuleListError) {
        let error = modules_named(names).expect_err("the param should not resolve");

        assert_eq!(error, expected);
    }

    #[test]
    fn unknown_module_error_lists_what_is_indexed() {
        let message = ModuleListError::Unknown("nope".to_string()).to_string();

        assert!(message.contains("`nope`"), "{message}");
        assert!(message.contains("dynamic_fee"), "{message}");
    }

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
