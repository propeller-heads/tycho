//! Algebra's farming proxy module: the gas it adds to `afterSwap`.
//!
//! With an incentive set, `afterSwap` moves the incentive's virtual pool to the pool's new tick
//! (`crossTo`), crossing the virtual pool's ticks on the way.

use std::collections::HashMap;

use alloy::primitives::Address;
use tycho_common::Bytes;

use crate::{
    evm::protocol::algebra_integral::plugin::{composed::GasModule, modules::field_bytes},
    protocol::errors::InvalidSnapshotError,
};

/// `crossTo` for a swap that crosses no tick: 2.8k in swaps of Nami's Base pool
/// `0x0E8AF367…7237` between blocks 61.0M and 61.7M.
const INCENTIVE_GAS: u64 = 3_000;
/// What `crossTo` adds for each pool tick the swap crosses. On the same pool and blocks, crossing
/// a farmed tick added 24.9k (66 swaps) or 59.4k when two reward slots went from zero to nonzero
/// (65 swaps), and crossing a tick no farmed position uses added nothing (14 swaps). Neither the
/// virtual pool's ticks nor their reward slots are indexed, so every crossed tick is charged the
/// cheaper farmed case. Over those 165 swaps this cuts the mean absolute error of the farming gas
/// from 33.4k (no per-tick charge) to 15.7k; it under-estimates the second case and
/// over-estimates crossings of unfarmed ticks.
///
/// Re-measure both figures when the farming contracts change.
const INCENTIVE_GAS_PER_CROSSED_TICK: u64 = 25_000;

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

    fn gas_per_crossed_tick(&self, _zero_for_one: bool) -> u64 {
        if self.incentive_set {
            INCENTIVE_GAS_PER_CROSSED_TICK
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
    #[case::incentive_set(
        attributes(&alloy::hex!("d877d4a9ffddd1b03fdfe709b9b0770a4d5dd3fe")),
        3_000,
        25_000
    )]
    #[case::incentive_cleared(attributes(&[0; 20]), 0, 0)]
    fn incentive_adds_gas(
        #[case] attributes: HashMap<String, Bytes>,
        #[case] gas: u64,
        #[case] gas_per_crossed_tick: u64,
    ) {
        let module = Farming::from_attributes(&attributes).expect("the incentive should decode");

        for zero_for_one in [true, false] {
            assert_eq!(module.gas(zero_for_one), gas);
            assert_eq!(module.gas_per_crossed_tick(zero_for_one), gas_per_crossed_tick);
        }
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
