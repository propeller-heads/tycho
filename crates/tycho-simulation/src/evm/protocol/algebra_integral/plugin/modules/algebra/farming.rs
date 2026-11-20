//! Algebra's farming proxy module: the gas it adds to `afterSwap`.
//!
//! With an incentive set, `afterSwap` moves the incentive's virtual pool to the pool's new tick.

use std::collections::HashMap;

use alloy::primitives::Address;
use tycho_common::Bytes;

use crate::{
    evm::protocol::algebra_integral::plugin::{composed::GasModule, modules::field_bytes},
    protocol::errors::InvalidSnapshotError,
};

/// Updating the incentive's virtual pool: about 60k in Base swap traces of Nami pools, for swaps
/// that crossed a tick. A swap that stays inside its tick spends less.
///
/// Re-measure when the farming contracts change.
const INCENTIVE_GAS: u64 = 60_000;

/// Whether a plugin's farming module has an incentive to update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Farming {
    incentive_set: bool,
}

impl Farming {
    /// Reads `farming/incentive`, which is zero while no incentive is set.
    ///
    /// Fails when the attribute is not an address.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        let incentive = Address::from(field_bytes::<20>(attributes, "farming/incentive")?);
        Ok(Self { incentive_set: !incentive.is_zero() })
    }
}

impl GasModule for Farming {
    fn gas(&self, _zero_for_one: bool) -> u64 {
        if self.incentive_set {
            INCENTIVE_GAS
        } else {
            0
        }
    }

    fn clone_box(&self) -> Box<dyn GasModule> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn attributes(incentive: &[u8]) -> HashMap<String, Bytes> {
        HashMap::from([("farming/incentive".to_string(), Bytes::from(incentive.to_vec()))])
    }

    #[rstest]
    // Plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at block 118127446.
    #[case::incentive_set(attributes(&alloy::hex!("d877d4a9ffddd1b03fdfe709b9b0770a4d5dd3fe")), 60_000)]
    #[case::incentive_cleared(attributes(&[0; 20]), 0)]
    fn incentive_adds_gas(#[case] attributes: HashMap<String, Bytes>, #[case] gas: u64) {
        let module = Farming::from_attributes(&attributes).expect("the incentive should decode");

        assert_eq!(module.gas(true), gas);
        assert_eq!(module.gas(false), gas);
    }

    #[test]
    fn wrong_width_is_rejected() {
        let error = Farming::from_attributes(&attributes(&[0x01; 32]))
            .expect_err("a 32-byte incentive should not decode");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("farming/incentive"), "{message}");
    }
}
