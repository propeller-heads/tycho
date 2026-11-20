//! The MEVX module of Nami's plugins: the gas of `afterSwap`.
//!
//! `afterSwap` asks the module's router for a backrun when one is set, and returns after its
//! checks otherwise.

use std::collections::HashMap;

use alloy::primitives::Address;
use tycho_common::Bytes;

use crate::{
    evm::protocol::algebra_integral::plugin::{composed::GasModule, modules::field_bytes},
    protocol::errors::InvalidSnapshotError,
};

/// `afterSwap` with no router set, including the pool's call into the plugin it already called
/// before the swap. Measured at 9.1k on Nami's Base WETH/USDC pool `0x0E8AF367…7237`, traced on
/// a fork at block 52562900.
const AFTER_SWAP_GAS: u64 = 9_000;
/// `afterSwap` with a router set whose backrun check finds nothing. Measured at 65.8k to 66.8k
/// on the same pool at block 52924436. When the check finds a backrun the plugin spends up to
/// its call gas budget more, which a quote does not count.
///
/// Re-measure both when the plugin implementation or the router changes.
const AFTER_SWAP_ROUTER_GAS: u64 = 66_000;

/// Whether a plugin's MEVX module has a router to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mevx {
    router_set: bool,
}

impl Mevx {
    /// Reads `mevx/router`, which is zero while no router is set.
    ///
    /// Fails when the attribute is not an address.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        let router = Address::from(field_bytes::<20>(attributes, "mevx/router")?);
        Ok(Self { router_set: !router.is_zero() })
    }
}

impl GasModule for Mevx {
    fn gas(&self, _zero_for_one: bool) -> u64 {
        if self.router_set {
            AFTER_SWAP_ROUTER_GAS
        } else {
            AFTER_SWAP_GAS
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

    fn attributes(router: &[u8]) -> HashMap<String, Bytes> {
        HashMap::from([("mevx/router".to_string(), Bytes::from(router.to_vec()))])
    }

    #[rstest]
    // Plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at block 118127446.
    #[case::router_set(attributes(&alloy::hex!("011242bf9f5c4832f152be5a5838daae40e0405c")), 66_000)]
    #[case::router_cleared(attributes(&[0; 20]), 9_000)]
    fn after_swap_gas_depends_on_the_router(
        #[case] attributes: HashMap<String, Bytes>,
        #[case] gas: u64,
    ) {
        let module = Mevx::from_attributes(&attributes).expect("the router should decode");

        assert_eq!(module.gas(true), gas);
        assert_eq!(module.gas(false), gas);
    }

    #[test]
    fn wrong_width_is_rejected() {
        let error = Mevx::from_attributes(&attributes(&[0x01; 32]))
            .expect_err("a 32-byte router should not decode");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("mevx/router"), "{message}");
    }
}
