//! Nami's fee override module, the `_swapFee` its plugins answer `beforeSwap` with.
//!
//! The fee starts from the dynamic fee module's. A plugin with a fee source enabled replaces it
//! by the fee its admin assigned to the source's current segment. With a cap set, a
//! per-direction premium is added, a keeper's override may replace the result until it expires
//! and then fades over the decay window, and the fee is held between the floor and the cap.
//!
//! The plugin answers 1 instead when the swap's sender is its MEVX executor. Quotes here are
//! for every other sender.

use std::collections::HashMap;

use alloy::primitives::Address;
use tycho_common::{
    simulation::{errors::SimulationError, protocol_sim::BlockContext},
    Bytes,
};

use crate::{
    evm::protocol::algebra_integral::plugin::{
        composed::{FeeModule, GasModule},
        modules::{algebra::dynamic_fee::DynamicFee, field_bytes, nami::fee_source::FeeSource},
        PluginFees,
    },
    protocol::errors::InvalidSnapshotError,
};

/// `beforeSwap` of a plugin with no fee source, including the pool's cold call into it: the
/// beacon read, the pool's `globalState`, the volatility oracle's timepoint for the block and the
/// fee. Measured at 64.4k inside the call on Nami's Base WETH/USDC plugin `0x3ea93f37…d6e5` at
/// block 59457000, plus the 2.6k of the cold call. A later swap in the same block finds the
/// timepoint written and spends 39.5k less, which a quote does not assume.
///
/// Re-measure when the plugin implementation changes.
const BEFORE_SWAP_GAS: u64 = 67_000;
/// What an enabled fee source adds to `beforeSwap`: two more storage words and the call to the
/// source. Measured as the 5.8k between the figure above and Robinhood plugin `0xb0918bdd…bb8a`
/// at block 118127446.
const FEE_SOURCE_GAS: u64 = 6_000;

/// Segment ids from here on have no fee of their own.
const MAX_SEGMENTS: usize = 16;
const BPS: u64 = 10_000;

/// A plugin's fee source and the fee assigned to each of its segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegmentFees {
    source: FeeSource,
    /// Hundredths of a bip by segment id; zero leaves the segment at the base fee.
    fees: [u16; MAX_SEGMENTS],
}

impl SegmentFees {
    fn fee_of(&self, id: u8, base: u64) -> u64 {
        match self.fees.get(usize::from(id)) {
            Some(0) | None => base,
            Some(fee) => u64::from(*fee),
        }
    }

    /// The current segment's fee, blended toward the next segment's during a transition.
    fn fee(&self, base: u64, timestamp: u64) -> Result<u64, SimulationError> {
        let segment = self.source.segment_at(timestamp)?;
        let fee = self.fee_of(segment.id, base);
        if segment.blend_bps == 0 || segment.next_id == segment.id {
            return Ok(fee);
        }
        let next = self.fee_of(segment.next_id, base);
        let blend = u64::from(segment.blend_bps);
        Ok(if fee > next {
            fee - (fee - next) * blend / BPS
        } else {
            fee + (next - fee) * blend / BPS
        })
    }
}

/// The fee of a Nami plugin whose dynamic fee is flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeOverride {
    base: DynamicFee,
    floor: u16,
    /// Zero turns premiums, overrides and bounds off.
    cap: u16,
    premium_zero_for_one: u16,
    premium_one_for_zero: u16,
    decay_window: u32,
    /// Zero means no override in that direction.
    fee_zero_for_one: u16,
    fee_one_for_zero: u16,
    expires_at: u64,
    max_discount_bps: u16,
    segments: Option<SegmentFees>,
}

impl FeeOverride {
    /// Reads the `dynamic_fee/*` and `fee_override/*` attributes.
    ///
    /// `sources` names the fee source contracts of the plugin's chain that can be reproduced.
    ///
    /// Fails when the dynamic fee does not decode, when an attribute is missing or has the wrong
    /// width, and when a source is enabled that is not in `sources`.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
        sources: &[(Address, FeeSource)],
    ) -> Result<Self, InvalidSnapshotError> {
        let base = DynamicFee::from_attributes(attributes)?;
        let u16_field = |name: &str| -> Result<u16, InvalidSnapshotError> {
            Ok(u16::from_be_bytes(field_bytes(attributes, name)?))
        };

        let mut expires_at = [0u8; 8];
        expires_at[3..].copy_from_slice(&field_bytes::<5>(attributes, "fee_override/expires_at")?);

        // Every field is read, whether or not the source is enabled: each is emitted when the
        // pool names its plugin, so a missing one means the pool is not fully indexed.
        let source_enabled = field_bytes::<1>(attributes, "fee_override/source_enabled")? != [0];
        let address = Address::from(field_bytes::<20>(attributes, "fee_override/source")?);
        let mut fees = [0u16; MAX_SEGMENTS];
        for (id, fee) in fees.iter_mut().enumerate() {
            *fee = u16_field(&format!("fee_override/segment_fee/{id}"))?;
        }
        let segments = if source_enabled {
            let mut known = None;
            for (candidate, source) in sources {
                if *candidate == address {
                    known = Some(*source);
                }
            }
            let Some(source) = known else {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "unsupported fee source {address}: its segments cannot be reproduced"
                )));
            };
            Some(SegmentFees { source, fees })
        } else {
            None
        };

        Ok(Self {
            base,
            floor: u16_field("fee_override/floor")?,
            cap: u16_field("fee_override/cap")?,
            premium_zero_for_one: u16_field("fee_override/premium_zero_for_one")?,
            premium_one_for_zero: u16_field("fee_override/premium_one_for_zero")?,
            decay_window: u32::from_be_bytes(field_bytes(attributes, "fee_override/decay_window")?),
            fee_zero_for_one: u16_field("fee_override/fee_zero_for_one")?,
            fee_one_for_zero: u16_field("fee_override/fee_one_for_zero")?,
            expires_at: u64::from_be_bytes(expires_at),
            max_discount_bps: u16_field("fee_override/max_discount_bps")?,
            segments,
        })
    }

    /// The fee of a swap in the given direction at `timestamp`, in hundredths of a bip.
    ///
    /// Fails where the contract reverts: a discount allowance above 100% with an override set,
    /// and a timestamp the fee source has no segment for.
    pub fn fee(&self, zero_for_one: bool, timestamp: u64) -> Result<u16, SimulationError> {
        let mut base = u64::from(self.base.fee());
        if let Some(segments) = &self.segments {
            base = segments.fee(base, timestamp)?;
        }

        let cap = u64::from(self.cap);
        if cap == 0 {
            return narrow(base);
        }

        let (premium, override_fee) = if zero_for_one {
            (self.premium_zero_for_one, self.fee_zero_for_one)
        } else {
            (self.premium_one_for_zero, self.fee_one_for_zero)
        };
        let side = base + u64::from(premium);
        let mut fee = side;

        if override_fee != 0 {
            // An override may price below the side fee only by the pool's allowed discount.
            let discount = side * u64::from(self.max_discount_bps) / BPS;
            let lowest = side
                .checked_sub(discount)
                .ok_or_else(|| {
                    SimulationError::RecoverableError(format!(
                        "the plugin reverts: a discount allowance of {} bps exceeds 100%",
                        self.max_discount_bps
                    ))
                })?;
            let override_fee = u64::from(override_fee).max(lowest);
            if override_fee != side {
                let decay_window = u64::from(self.decay_window);
                let decay_end = self.expires_at + decay_window;
                if timestamp < self.expires_at {
                    fee = override_fee;
                } else if timestamp < decay_end {
                    // Fades back toward the side fee from either direction.
                    let remaining = decay_end - timestamp;
                    fee = if override_fee > side {
                        side + (override_fee - side) * remaining / decay_window
                    } else {
                        side - (side - override_fee) * remaining / decay_window
                    };
                }
            }
        }

        let floor = u64::from(self.floor);
        if fee < floor {
            fee = floor;
        } else if fee > cap {
            fee = cap;
        }
        narrow(fee)
    }
}

/// The contract returns its fee as a `uint16`. Every path keeps it in range, so a wider value
/// means this port has diverged from it.
fn narrow(fee: u64) -> Result<u16, SimulationError> {
    u16::try_from(fee).map_err(|error| {
        SimulationError::FatalError(format!("fee {fee} does not fit the plugin's uint16: {error}"))
    })
}

impl FeeModule for FeeOverride {
    /// The pool reads a zero from the plugin as no override and keeps the fee it last stored.
    fn fees(
        &self,
        zero_for_one: bool,
        block: &BlockContext,
    ) -> Result<PluginFees, SimulationError> {
        let fee = self.fee(zero_for_one, block.timestamp())?;
        Ok(PluginFees { override_fee: (fee != 0).then_some(u32::from(fee)), plugin_fee: 0 })
    }

    fn clone_box(&self) -> Box<dyn FeeModule> {
        Box::new(*self)
    }
}

impl GasModule for FeeOverride {
    fn gas(&self, _zero_for_one: bool) -> u64 {
        if self.segments.is_some() {
            BEFORE_SWAP_GAS + FEE_SOURCE_GAS
        } else {
            BEFORE_SWAP_GAS
        }
    }

    fn clone_box(&self) -> Box<dyn GasModule> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use rstest::rstest;

    use super::*;

    /// `NyseFeeSource` on Base.
    const NYSE: Address = address!("146669E5670c08730599ED8182AC979c5d04E5d2");
    /// `GoldFeeSource` on Ethereum.
    const GOLD: Address = address!("A7f5e79C7c88A917993dd4785dA700214C4429a6");
    const SOURCES: [(Address, FeeSource); 2] = [(NYSE, FeeSource::Nyse), (GOLD, FeeSource::Gold)];

    /// Monday 6 July 2026 15:00 UTC, the NYSE session (segment 2) and gold's active hours.
    const SESSION: u64 = 1_783_350_000;
    /// Saturday 11 July 2026 12:00 UTC, closed (segment 0) on both sources.
    const WEEKEND: u64 = 1_783_771_200;
    /// Monday 6 July 2026 14:00 UTC, half way through the opening spike after a closure
    /// (segment 4 blending into 2).
    const OPENING: u64 = 1_783_346_400;

    #[derive(Debug, Clone, Default)]
    struct Config {
        base: u16,
        floor: u16,
        cap: u16,
        premium_zero_for_one: u16,
        premium_one_for_zero: u16,
        decay_window: u32,
        fee_zero_for_one: u16,
        fee_one_for_zero: u16,
        expires_at: u64,
        source: Option<Address>,
        max_discount_bps: u16,
        segment_fees: [u16; MAX_SEGMENTS],
    }

    impl Config {
        fn attributes(&self) -> HashMap<String, Bytes> {
            let mut attributes = HashMap::new();
            let mut set = |name: &str, bytes: &[u8]| {
                attributes.insert(name.to_string(), Bytes::from(bytes.to_vec()));
            };
            set("dynamic_fee/alpha1", &[0, 0]);
            set("dynamic_fee/alpha2", &[0, 0]);
            set("dynamic_fee/base_fee", &self.base.to_be_bytes());
            set("fee_override/floor", &self.floor.to_be_bytes());
            set("fee_override/cap", &self.cap.to_be_bytes());
            set("fee_override/premium_zero_for_one", &self.premium_zero_for_one.to_be_bytes());
            set("fee_override/premium_one_for_zero", &self.premium_one_for_zero.to_be_bytes());
            set("fee_override/decay_window", &self.decay_window.to_be_bytes());
            set("fee_override/fee_zero_for_one", &self.fee_zero_for_one.to_be_bytes());
            set("fee_override/fee_one_for_zero", &self.fee_one_for_zero.to_be_bytes());
            set("fee_override/expires_at", &self.expires_at.to_be_bytes()[3..]);
            set("fee_override/max_discount_bps", &self.max_discount_bps.to_be_bytes());
            set("fee_override/source_enabled", &[u8::from(self.source.is_some())]);
            set(
                "fee_override/source",
                self.source
                    .unwrap_or_default()
                    .as_slice(),
            );
            for (id, fee) in self.segment_fees.iter().enumerate() {
                set(&format!("fee_override/segment_fee/{id}"), &fee.to_be_bytes());
            }
            attributes
        }

        fn module(&self) -> FeeOverride {
            FeeOverride::from_attributes(&self.attributes(), &SOURCES)
                .expect("the configuration should decode")
        }
    }

    /// A row of the reference: the configuration, the swap and what `_swapFee` answered.
    fn reference_row(line: &str) -> (Config, bool, u64, Option<u16>) {
        let fields: Vec<&str> = line.split(',').collect();
        let [base, floor, cap, premium_zero_for_one, premium_one_for_zero, decay_window, fee_zero_for_one, fee_one_for_zero, expires_at, source, max_discount_bps, segment_fees, zero_for_one, timestamp, fee] =
            fields.as_slice()
        else {
            panic!("malformed reference row {line:?}");
        };
        let number = |field: &str| -> u64 {
            field
                .parse()
                .expect("a reference field should be a number")
        };
        let narrow = |field: &str| -> u16 {
            u16::try_from(number(field)).expect("a reference fee should fit a u16")
        };
        // The word is big-endian and segment 0 sits in its lowest 16 bits.
        let word = alloy::hex::decode(segment_fees).expect("segment fees should be hex");
        let mut fees = [0u16; MAX_SEGMENTS];
        for (id, fee) in fees.iter_mut().enumerate() {
            let end = word.len() - 2 * id;
            *fee = u16::from_be_bytes([word[end - 2], word[end - 1]]);
        }
        let config = Config {
            base: narrow(base),
            floor: narrow(floor),
            cap: narrow(cap),
            premium_zero_for_one: narrow(premium_zero_for_one),
            premium_one_for_zero: narrow(premium_one_for_zero),
            decay_window: u32::try_from(number(decay_window))
                .expect("a decay window should fit a u32"),
            fee_zero_for_one: narrow(fee_zero_for_one),
            fee_one_for_zero: narrow(fee_one_for_zero),
            expires_at: number(expires_at),
            source: match *source {
                "0" => None,
                "1" => Some(NYSE),
                "2" => Some(GOLD),
                other => panic!("unknown source kind {other}"),
            },
            max_discount_bps: narrow(max_discount_bps),
            segment_fees: fees,
        };
        let expected = if *fee == "revert" { None } else { Some(narrow(fee)) };
        (config, *zero_for_one == "1", number(timestamp), expected)
    }

    /// The reference is `_swapFee`, copied verbatim into a Foundry harness that stands in for
    /// the dynamic fee, run on 600 pseudo-random configurations with both fee sources.
    #[test]
    fn fee_matches_the_contract() {
        let mut reverts = 0;
        for line in include_str!("testdata/fee_override_vectors.csv").lines() {
            let (config, zero_for_one, timestamp, expected) = reference_row(line);

            let fee = config
                .module()
                .fee(zero_for_one, timestamp);

            match expected {
                Some(expected) => {
                    assert_eq!(fee.expect("the contract answers this row"), expected, "{line}")
                }
                None => {
                    reverts += 1;
                    assert!(fee.is_err(), "the contract reverts on {line}, got {fee:?}");
                }
            }
        }
        assert!(reverts > 0, "the reference should include reverting rows");
    }

    #[test]
    fn plugin_with_untouched_storage_charges_the_base_fee() {
        let attributes = Config { base: 500, ..Default::default() }.attributes();

        let module = FeeOverride::from_attributes(&attributes, &SOURCES)
            .expect("an all-zero fee override should decode");

        assert_eq!(
            module
                .fee(true, SESSION)
                .expect("a fee"),
            500
        );
        assert_eq!(
            module
                .fee(false, SESSION)
                .expect("a fee"),
            500
        );
    }

    #[test]
    fn missing_field_is_rejected() {
        let mut attributes = Config { base: 500, ..Default::default() }.attributes();
        attributes.remove("fee_override/segment_fee/15");

        let error = FeeOverride::from_attributes(&attributes, &SOURCES)
            .expect_err("every field should be required");

        assert!(
            matches!(&error, InvalidSnapshotError::MissingAttribute(name) if name == "fee_override/segment_fee/15"),
            "{error:?}"
        );
    }

    #[rstest]
    // Plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood: floor 500, cap 10000,
    // a 4 hour decay window and nothing else.
    #[case::bounds_only(Config { base: 500, floor: 500, cap: 10_000, decay_window: 14_400, ..Default::default() }, 500, 500)]
    #[case::floor_lifts_the_fee(Config { base: 100, floor: 300, cap: 10_000, ..Default::default() }, 300, 300)]
    #[case::cap_lowers_the_fee(Config { base: 9_000, cap: 5_000, ..Default::default() }, 5_000, 5_000)]
    #[case::premium_per_direction(Config { base: 500, cap: 10_000, premium_zero_for_one: 200, premium_one_for_zero: 50, ..Default::default() }, 700, 550)]
    #[case::no_cap_ignores_premium_and_floor(Config { base: 500, floor: 900, premium_zero_for_one: 200, fee_zero_for_one: 3_000, expires_at: u64::from(u32::MAX), ..Default::default() }, 500, 500)]
    fn fee_without_an_override(
        #[case] config: Config,
        #[case] zero_for_one: u16,
        #[case] one_for_zero: u16,
    ) {
        let module = config.module();

        assert_eq!(
            module
                .fee(true, SESSION)
                .expect("a fee"),
            zero_for_one
        );
        assert_eq!(
            module
                .fee(false, SESSION)
                .expect("a fee"),
            one_for_zero
        );
    }

    /// Base 1000, an override of `fee` for zero-for-one swaps expiring at `SESSION` and fading
    /// over the next 1000 seconds.
    fn overridden(fee: u16, max_discount_bps: u16) -> Config {
        Config {
            base: 1_000,
            cap: 60_000,
            fee_zero_for_one: fee,
            expires_at: SESSION,
            decay_window: 1_000,
            max_discount_bps,
            ..Default::default()
        }
    }

    #[rstest]
    #[case::live_above(overridden(3_000, 0), SESSION - 1, 3_000)]
    #[case::decay_starts_at_expiry(overridden(3_000, 0), SESSION, 3_000)]
    #[case::decayed_a_quarter(overridden(3_000, 0), SESSION + 250, 2_500)]
    #[case::decay_last_second(overridden(3_000, 0), SESSION + 999, 1_002)]
    #[case::decay_over(overridden(3_000, 0), SESSION + 1_000, 1_000)]
    #[case::below_without_allowance_is_lifted(overridden(400, 0), SESSION - 1, 1_000)]
    #[case::below_within_allowance(overridden(800, 5_000), SESSION - 1, 800)]
    #[case::below_past_allowance_is_lifted(overridden(400, 5_000), SESSION - 1, 500)]
    #[case::discount_decays_upward(overridden(800, 5_000), SESSION + 250, 850)]
    fn override_applies_then_fades(
        #[case] config: Config,
        #[case] timestamp: u64,
        #[case] expected: u16,
    ) {
        let module = config.module();

        assert_eq!(
            module
                .fee(true, timestamp)
                .expect("a fee"),
            expected
        );
        assert_eq!(
            module
                .fee(false, timestamp)
                .expect("a fee"),
            1_000,
            "the other direction has no override"
        );
    }

    #[test]
    fn discount_larger_than_the_fee_reverts_only_with_an_override() {
        let module = overridden(800, 20_000).module();

        let error = module
            .fee(true, SESSION - 1)
            .expect_err("the contract underflows");
        let SimulationError::RecoverableError(message) = error else {
            panic!("expected a recoverable error, got {error:?}");
        };
        assert!(message.contains("20000 bps"), "{message}");
        assert_eq!(
            module
                .fee(false, SESSION - 1)
                .expect("a fee"),
            1_000
        );
    }

    fn with_source(source: Address) -> Config {
        let mut segment_fees = [0u16; MAX_SEGMENTS];
        segment_fees[0] = 50_000;
        segment_fees[2] = 400;
        segment_fees[4] = 2_400;
        Config { base: 500, source: Some(source), segment_fees, ..Default::default() }
    }

    #[rstest]
    #[case::session(SESSION, 400)]
    #[case::closed(WEEKEND, 50_000)]
    // Half way from 2400 to 400.
    #[case::blending(OPENING, 1_400)]
    // Monday 6 July 2026 08:00 UTC is 04:00 in New York: segment 6, which has no fee assigned.
    #[case::unassigned_segment_uses_the_base(1_783_324_800, 500)]
    fn source_picks_the_segment_fee(#[case] timestamp: u64, #[case] expected: u16) {
        let module = with_source(NYSE).module();

        assert_eq!(
            module
                .fee(true, timestamp)
                .expect("a fee"),
            expected
        );
    }

    #[test]
    fn segment_fee_is_bounded_like_the_base_fee() {
        let config = Config { floor: 500, cap: 10_000, ..with_source(NYSE) };
        let module = config.module();

        assert_eq!(
            module
                .fee(true, WEEKEND)
                .expect("a fee"),
            10_000
        );
        assert_eq!(
            module
                .fee(true, SESSION)
                .expect("a fee"),
            500
        );
    }

    #[test]
    fn enabled_source_that_is_not_known_is_rejected() {
        let unknown = address!("00000000000000000000000000000000000000f5");

        let error = FeeOverride::from_attributes(&with_source(unknown).attributes(), &SOURCES)
            .expect_err("an unknown source should not decode");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains(&unknown.to_string()), "{message}");
    }

    #[test]
    fn disabled_source_is_ignored_whatever_its_address() {
        let mut attributes = with_source(NYSE).attributes();
        attributes.insert("fee_override/source_enabled".to_string(), Bytes::from(vec![0]));
        attributes.insert("fee_override/source".to_string(), Bytes::from(vec![0xf5; 20]));

        let module = FeeOverride::from_attributes(&attributes, &SOURCES)
            .expect("a disabled source should not be looked up");

        assert_eq!(
            module
                .fee(true, WEEKEND)
                .expect("a fee"),
            500
        );
    }

    #[test]
    fn wrong_width_is_rejected() {
        let mut attributes = Config { base: 500, ..Default::default() }.attributes();
        attributes.insert("fee_override/expires_at".to_string(), Bytes::from(vec![0; 8]));

        let error = FeeOverride::from_attributes(&attributes, &SOURCES)
            .expect_err("an eight-byte expiry should not decode");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("fee_override/expires_at"), "{message}");
    }

    #[test]
    fn adaptive_dynamic_fee_is_rejected() {
        let mut attributes = Config { base: 500, ..Default::default() }.attributes();
        attributes.insert("dynamic_fee/alpha1".to_string(), Bytes::from(vec![0x05, 0xdc]));

        let error = FeeOverride::from_attributes(&attributes, &SOURCES)
            .expect_err("an adaptive fee should not decode");

        assert!(matches!(error, InvalidSnapshotError::ValueError(_)), "{error:?}");
    }

    #[rstest]
    #[case::fee_is_an_override(500, Some(500))]
    #[case::zero_keeps_the_pool_s_fee(0, None)]
    fn fee_module_reports_the_override(#[case] base: u16, #[case] override_fee: Option<u32>) {
        let module = Config { base, ..Default::default() }.module();

        let fees = module
            .fees(true, &BlockContext::new(1, SESSION))
            .expect("fees");

        assert_eq!(fees, PluginFees { override_fee, plugin_fee: 0 });
    }

    #[rstest]
    #[case::no_source(Config { base: 500, ..Default::default() }, 67_000)]
    #[case::source_enabled(with_source(NYSE), 73_000)]
    fn before_swap_gas_counts_the_source(#[case] config: Config, #[case] gas: u64) {
        let module = config.module();

        assert_eq!(GasModule::gas(&module, true), gas);
        assert_eq!(GasModule::gas(&module, false), gas);
    }
}
