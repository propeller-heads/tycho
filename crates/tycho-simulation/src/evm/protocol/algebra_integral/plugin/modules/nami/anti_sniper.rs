//! Nami's AntiSniper module, as a gate.
//!
//! While its period runs, the module pulls an extra fee from whoever calls the pool, with a
//! token transfer of its own. The simulation does not reproduce that fee: it offers no
//! liquidity until the module charges nothing. That is the case once the period has ended, and
//! always when the module has no starting fee.
//!
//! The module exempts the senders its admin lists. Quotes here are for every other sender.

use std::collections::HashMap;

use tycho_common::{simulation::protocol_sim::BlockContext, Bytes};

use crate::{
    evm::protocol::algebra_integral::plugin::{
        composed::{GasModule, GateModule},
        modules::field_bytes,
    },
    protocol::errors::InvalidSnapshotError,
};

/// What the module's check adds to `beforeSwap` once its period is over. Measured as the 8.9k
/// between Nami's Base plugins `0x53ce1d10…5fea` (with the module) and `0x3ea93f37…d6e5`
/// (without) at block 59457000.
///
/// Re-measure when the plugin implementation changes.
const CHECK_GAS: u64 = 9_000;

/// The part of a plugin's AntiSniper configuration that decides when it stops charging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AntiSniper {
    /// Zero disables the module.
    starting_fee: u32,
    /// Seconds the fee takes to decay to nothing.
    period: u32,
    /// Zero until the admin starts the period.
    start_timestamp: u64,
}

impl AntiSniper {
    /// Reads the `anti_sniper/*` attributes.
    ///
    /// Fails when an attribute has the wrong width.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        let mut starting_fee = [0u8; 4];
        starting_fee[1..]
            .copy_from_slice(&field_bytes::<3>(attributes, "anti_sniper/starting_fee")?);
        Ok(Self {
            starting_fee: u32::from_be_bytes(starting_fee),
            period: u32::from_be_bytes(field_bytes(attributes, "anti_sniper/period")?),
            start_timestamp: u64::from_be_bytes(field_bytes(
                attributes,
                "anti_sniper/start_timestamp",
            )?),
        })
    }
}

impl GateModule for AntiSniper {
    fn swaps_enabled(&self, block: &BlockContext) -> bool {
        if self.starting_fee == 0 {
            return true;
        }
        if self.start_timestamp == 0 {
            return false;
        }
        block.timestamp() >=
            self.start_timestamp
                .saturating_add(u64::from(self.period))
    }

    fn clone_box(&self) -> Box<dyn GateModule> {
        Box::new(*self)
    }
}

impl GasModule for AntiSniper {
    fn gas(&self, _zero_for_one: bool) -> u64 {
        CHECK_GAS
    }

    fn clone_box(&self) -> Box<dyn GasModule> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const START: u64 = 1_794_499_200;
    const PERIOD: u32 = 1_800;

    fn attributes(starting_fee: u32, period: u32, start: u64) -> HashMap<String, Bytes> {
        HashMap::from([
            (
                "anti_sniper/starting_fee".to_string(),
                Bytes::from(starting_fee.to_be_bytes()[1..].to_vec()),
            ),
            ("anti_sniper/period".to_string(), Bytes::from(period.to_be_bytes().to_vec())),
            ("anti_sniper/start_timestamp".to_string(), Bytes::from(start.to_be_bytes().to_vec())),
        ])
    }

    fn enabled_at(attributes: &HashMap<String, Bytes>, timestamp: u64) -> bool {
        AntiSniper::from_attributes(attributes)
            .expect("the configuration should decode")
            .swaps_enabled(&BlockContext::new(1, timestamp))
    }

    #[rstest]
    // Plugin 0x53ce1d109b8ffe35010baaac91a3d839f4b05fea on Base at block 59132137.
    #[case::before_the_start(START - 1, false)]
    #[case::at_the_start(START, false)]
    #[case::last_second_of_the_period(START + 1_799, false)]
    #[case::period_over(START + 1_800, true)]
    #[case::long_after(START + 1_000_000, true)]
    fn gate_opens_when_the_period_ends(#[case] timestamp: u64, #[case] enabled: bool) {
        assert_eq!(enabled_at(&attributes(832_986, PERIOD, START), timestamp), enabled);
    }

    #[test]
    fn configured_but_never_started_stays_shut() {
        assert!(!enabled_at(&attributes(832_986, PERIOD, 0), u64::MAX));
    }

    #[rstest]
    #[case::started(START)]
    #[case::never_started(0)]
    fn module_without_a_starting_fee_never_gates(#[case] start: u64) {
        assert!(enabled_at(&attributes(0, PERIOD, start), START));
    }

    #[test]
    fn missing_field_is_rejected() {
        let mut attributes = attributes(832_986, PERIOD, START);
        attributes.remove("anti_sniper/start_timestamp");

        let error =
            AntiSniper::from_attributes(&attributes).expect_err("every field should be required");

        assert!(
            matches!(&error, InvalidSnapshotError::MissingAttribute(name) if name == "anti_sniper/start_timestamp"),
            "{error:?}"
        );
    }

    #[test]
    fn wrong_width_is_rejected() {
        let mut attributes = attributes(832_986, PERIOD, START);
        attributes.insert("anti_sniper/period".to_string(), Bytes::from(vec![0x07, 0x08]));

        let error = AntiSniper::from_attributes(&attributes)
            .expect_err("a two-byte period should not decode");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("anti_sniper/period"), "{message}");
    }

    #[test]
    fn check_costs_gas_in_both_directions() {
        let module = AntiSniper::from_attributes(&attributes(832_986, PERIOD, START))
            .expect("the configuration should decode");

        assert_eq!(module.gas(true), 9_000);
        assert_eq!(module.gas(false), 9_000);
    }
}
