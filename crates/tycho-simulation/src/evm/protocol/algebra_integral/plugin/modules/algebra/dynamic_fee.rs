//! Algebra's dynamic fee module, for configurations whose fee does not adapt.
//!
//! The module's fee is `baseFee` plus two sigmoids of recent volatility, of heights `alpha1` and
//! `alpha2`. With both heights zero the fee is `baseFee` whatever the volatility, and that is
//! the only case handled here: the adaptive case needs the volatility oracle's timepoints.

use std::collections::HashMap;

use tycho_common::Bytes;

use crate::{
    evm::protocol::algebra_integral::plugin::modules::field_bytes,
    protocol::errors::InvalidSnapshotError,
};

/// A dynamic fee module configured with a flat fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynamicFee {
    base_fee: u16,
}

impl DynamicFee {
    /// Reads the module's `dynamic_fee/*` attributes.
    ///
    /// Fails when one is missing or malformed, and when either sigmoid height is non-zero: such a
    /// pool charges a fee this type cannot compute.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        let alpha1 = u16::from_be_bytes(field_bytes(attributes, "dynamic_fee/alpha1")?);
        let alpha2 = u16::from_be_bytes(field_bytes(attributes, "dynamic_fee/alpha2")?);
        let base_fee = u16::from_be_bytes(field_bytes(attributes, "dynamic_fee/base_fee")?);
        if alpha1 != 0 || alpha2 != 0 {
            return Err(InvalidSnapshotError::ValueError(format!(
                "unsupported dynamic fee configuration: alpha1 {alpha1} and alpha2 {alpha2} make \
                 the fee adapt to volatility"
            )));
        }
        Ok(Self { base_fee })
    }

    /// The module's fee, in hundredths of a bip.
    pub fn fee(&self) -> u32 {
        u32::from(self.base_fee)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn attributes(alpha1: u16, alpha2: u16, base_fee: u16) -> HashMap<String, Bytes> {
        HashMap::from([
            ("dynamic_fee/alpha1".to_string(), Bytes::from(alpha1.to_be_bytes().to_vec())),
            ("dynamic_fee/alpha2".to_string(), Bytes::from(alpha2.to_be_bytes().to_vec())),
            ("dynamic_fee/base_fee".to_string(), Bytes::from(base_fee.to_be_bytes().to_vec())),
        ])
    }

    #[rstest]
    // Plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at block 116347875.
    #[case::deployed_config(500)]
    #[case::zero(0)]
    #[case::max(u16::MAX)]
    fn flat_configuration_charges_the_base_fee(#[case] base_fee: u16) {
        let module = DynamicFee::from_attributes(&attributes(0, 0, base_fee))
            .expect("a flat configuration should decode");

        assert_eq!(module.fee(), u32::from(base_fee));
    }

    #[rstest]
    #[case::alpha1(1, 0)]
    #[case::alpha2(0, 1)]
    #[case::both(2900, 12000)]
    fn adaptive_configuration_is_rejected(#[case] alpha1: u16, #[case] alpha2: u16) {
        let error = DynamicFee::from_attributes(&attributes(alpha1, alpha2, 500))
            .expect_err("an adaptive configuration should not decode");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains(&format!("alpha1 {alpha1}")), "{message}");
        assert!(message.contains(&format!("alpha2 {alpha2}")), "{message}");
    }

    #[rstest]
    #[case::alpha1("dynamic_fee/alpha1")]
    #[case::alpha2("dynamic_fee/alpha2")]
    #[case::base_fee("dynamic_fee/base_fee")]
    fn missing_attribute_is_rejected(#[case] missing: &str) {
        let mut attributes = attributes(0, 0, 500);
        attributes.remove(missing);

        let error = DynamicFee::from_attributes(&attributes)
            .expect_err("an incomplete configuration should not decode");

        let InvalidSnapshotError::MissingAttribute(name) = error else {
            panic!("expected a missing attribute, got {error:?}");
        };
        assert_eq!(name, missing);
    }
}
