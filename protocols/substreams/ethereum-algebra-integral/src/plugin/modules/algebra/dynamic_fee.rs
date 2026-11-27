//! Algebra's dynamic fee module (`DynamicFeeStorage`, `AlgebraFeeConfigurationU144`).
//!
//! The module keeps one packed fee configuration. Only the fields the simulation reads are
//! indexed: the base fee, and the two sigmoid heights that say whether the fee is flat (both
//! zero) or adapts to volatility.

use hex_literal::hex;

use crate::plugin::{modules::PluginModule, storage::StorageField};

/// `DynamicFeeStorage.NAMESPACE`, where `Layout.feeConfig` sits. The constant is the one in the
/// contract; it is not what the ERC-7201 formula in the contract's comment evaluates to.
const FEE_CONFIG_SLOT: [u8; 32] =
    hex!("fbbf1a562c70d290e080160018965a1e5db682cf55e666eca8f391a4ceef9a00");

pub const MODULE: PluginModule = PluginModule {
    name: "dynamic_fee",
    fields: &[
        // uint16 at ALPHA1_OFFSET = 0 bits.
        StorageField::new("dynamic_fee/alpha1", FEE_CONFIG_SLOT, 0, 2),
        // uint16 at ALPHA2_OFFSET = 16 bits.
        StorageField::new("dynamic_fee/alpha2", FEE_CONFIG_SLOT, 2, 2),
        // uint16 at BASE_FEE_OFFSET = 128 bits, in hundredths of a bip.
        StorageField::new("dynamic_fee/base_fee", FEE_CONFIG_SLOT, 16, 2),
    ],
};

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::StorageChange;

    use super::*;
    use crate::plugin::attributes::creation_attributes;

    /// `feeConfig` of plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at block
    /// 116347875: base fee 500, both gammas 1, alphas and betas 0.
    const ROBINHOOD_FEE_CONFIG: [u8; 32] =
        hex!("000000000000000000000000000001f400010001000000000000000000000000");

    #[test]
    fn decodes_a_deployed_fee_config() {
        let write = StorageChange {
            key: FEE_CONFIG_SLOT.to_vec(),
            new_value: ROBINHOOD_FEE_CONFIG.to_vec(),
            ..Default::default()
        };

        let attributes = creation_attributes(&[write], &[&MODULE]);

        let decoded: Vec<(&str, &[u8])> = attributes
            .iter()
            .map(|attribute| (attribute.name.as_str(), attribute.value.as_slice()))
            .collect();
        assert_eq!(
            decoded,
            [
                ("dynamic_fee/alpha1", hex!("0000").as_slice()),
                ("dynamic_fee/alpha2", hex!("0000").as_slice()),
                ("dynamic_fee/base_fee", hex!("01f4").as_slice()),
            ]
        );
    }
}
