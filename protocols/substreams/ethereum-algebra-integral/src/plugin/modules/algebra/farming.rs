//! Algebra's farming proxy module (`FarmingProxyStorage`).
//!
//! With an incentive set, the plugin's `afterSwap` moves the incentive's virtual pool to the new
//! tick, which costs gas on every swap. The incentive's address is all the simulation reads.

use hex_literal::hex;

use crate::plugin::{modules::PluginModule, storage::StorageField};

/// `FarmingProxyStorage.NAMESPACE`, where `Layout.incentive` sits.
const INCENTIVE_SLOT: [u8; 32] =
    hex!("defe014d06c76a87f6a6efbd410e650d6bcbb85e816546b55f626b436c8a1000");

pub const MODULE: PluginModule = PluginModule {
    name: "farming",
    fields: &[StorageField::new("farming/incentive", INCENTIVE_SLOT, 0, 20)],
};

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::StorageChange;

    use super::*;
    use crate::plugin::attributes::creation_attributes;

    /// `incentive` of plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at block
    /// 118127446.
    #[test]
    fn decodes_a_deployed_incentive() {
        let write = StorageChange {
            key: INCENTIVE_SLOT.to_vec(),
            new_value: hex!("000000000000000000000000d877d4a9ffddd1b03fdfe709b9b0770a4d5dd3fe")
                .to_vec(),
            ..Default::default()
        };

        let attributes = creation_attributes(&[write], &[&MODULE]);

        let decoded: Vec<(&str, &[u8])> = attributes
            .iter()
            .map(|attribute| (attribute.name.as_str(), attribute.value.as_slice()))
            .collect();
        assert_eq!(
            decoded,
            [("farming/incentive", hex!("d877d4a9ffddd1b03fdfe709b9b0770a4d5dd3fe").as_slice())]
        );
    }
}
