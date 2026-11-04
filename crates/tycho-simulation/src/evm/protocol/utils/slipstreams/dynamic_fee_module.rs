use std::collections::HashMap;

use alloy::primitives::{address, Address};
use serde::{Deserialize, Serialize};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::protocol::utils::slipstreams::observations::{Observations, ObserveError};

pub const ZERO_FEE_INDICATOR: u32 = 420;
// The `secondsAgo` the dynamic fee modules are deployed with, and the one a config serialized
// before it became per module takes. `SUPPORTED_DYNAMIC_FEE_MODULES` holds each module's own.
pub const DEFAULT_SECONDS_AGO: u32 = 600;
pub const MIN_SECONDS_AGO: u32 = 2;
pub const DEFAULT_SCALING_FACTOR: u64 = 0;
// The upgraded DynamicSwapFeeModule deployments use 30_000 as their default fee cap.
pub const DEFAULT_FEE_CAP: u32 = 30_000;
const SCALING_PRECISION: u128 = 1_000_000;
const DYNAMIC_FEE_MODULE_ATTRIBUTE: &str = "dynamic_fee_module";

/// A deployed fee module whose `getFee` `get_dynamic_fee` reproduces.
struct SupportedModule {
    address: Address,
    /// The module's `secondsAgo`: the TWAP window, which also sets the minimum observation
    /// cardinality (`secondsAgo / MIN_SECONDS_AGO`) below which the module adds no dynamic fee.
    /// A snapshot of state the swap fee manager can change with `setSecondsAgo`, which emits
    /// `SecondsAgoSet`; nothing indexes it, so a change needs a code change here.
    seconds_ago: u32,
}

const SUPPORTED_DYNAMIC_FEE_MODULES: [SupportedModule; 4] = [
    // The three Base modules moved from 600 to 480 in tx 0x42122c80…7550 (block 52,307,488), and
    // 0x87D8 again to 30 in tx 0x1aeb7f17…2ae4 (block 53,228,614).
    SupportedModule {
        address: address!("090b2A6bb475c00e2256e2095A60887cD710803b"),
        seconds_ago: 480,
    },
    SupportedModule {
        address: address!("F4Ecd78EBEB6d36CF7f80B5B6B41453515fe2785"),
        seconds_ago: 480,
    },
    SupportedModule {
        address: address!("87D8f999BBa9343E8099552426775B51C338E8CB"),
        seconds_ago: 30,
    },
    // UP on Robinhood Chain. Same module code as the Base deployments above, so the fee resolution
    // below applies unchanged; only its `factory` immutable and the name of its base-fee event
    // differ.
    SupportedModule {
        address: address!("a8Bdc945bE050E451C97f935d7c0D6B0087cF94c"),
        seconds_ago: DEFAULT_SECONDS_AGO,
    },
];

fn supported_dynamic_fee_module(fee_module: &[u8]) -> Option<&'static SupportedModule> {
    SUPPORTED_DYNAMIC_FEE_MODULES
        .iter()
        .find(|supported| supported.address.as_slice() == fee_module)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicFeeConfig {
    base_fee: u32,
    fee_cap: u32,
    scaling_factor: u64,
    initial_fee_enabled: bool,
    initial_fee: u32,
    /// The pool's fee module's `secondsAgo`.
    #[serde(default = "default_seconds_ago")]
    seconds_ago: u32,
}

fn default_seconds_ago() -> u32 {
    DEFAULT_SECONDS_AGO
}

impl Default for DynamicFeeConfig {
    fn default() -> Self {
        Self::new(0, 0, 0, false, 0)
    }
}

impl DynamicFeeConfig {
    pub fn new(
        base_fee: u32,
        fee_cap: u32,
        scaling_factor: u64,
        initial_fee_enabled: bool,
        initial_fee: u32,
    ) -> Self {
        Self {
            base_fee,
            fee_cap,
            scaling_factor,
            initial_fee_enabled,
            initial_fee,
            seconds_ago: DEFAULT_SECONDS_AGO,
        }
    }

    /// The same config under a module whose `secondsAgo` is `seconds_ago`.
    #[cfg(test)]
    pub(crate) fn with_seconds_ago(self, seconds_ago: u32) -> Self {
        Self { seconds_ago, ..self }
    }

    /// Builds a config from a full snapshot's attributes.
    ///
    /// An absent or unsupported `dynamic_fee_module` marker yields the default config, whose
    /// `base_fee == 0` makes `get_dynamic_fee` fall back to the pool's static `default_fee`. Only
    /// errors when a supported marker is present but a dynamic-fee attribute is missing.
    pub(crate) fn from_attributes(attributes: &HashMap<String, Bytes>) -> Result<Self, String> {
        let Some(module) = attributes
            .get(DYNAMIC_FEE_MODULE_ATTRIBUTE)
            .and_then(|module| supported_dynamic_fee_module(module))
        else {
            return Ok(Self::default());
        };

        Ok(Self {
            base_fee: u32::try_from(
                attributes
                    .get("dfc_baseFee")
                    .ok_or("missing dynamic fee attribute `dfc_baseFee`")?
                    .clone(),
            )
            .map_err(|err| err.to_string())?,
            fee_cap: u32::try_from(
                attributes
                    .get("dfc_feeCap")
                    .ok_or("missing dynamic fee attribute `dfc_feeCap`")?
                    .clone(),
            )
            .map_err(|err| err.to_string())?,
            scaling_factor: u64::try_from(
                attributes
                    .get("dfc_scalingFactor")
                    .ok_or("missing dynamic fee attribute `dfc_scalingFactor`")?
                    .clone(),
            )
            .map_err(|err| err.to_string())?,
            initial_fee_enabled: attributes
                .get("dfc_initialFeeEnabled")
                .is_some_and(|value| value.iter().any(|byte| *byte != 0)),
            initial_fee: attributes
                .get("dfc_initialFee")
                .cloned()
                .map(u32::try_from)
                .transpose()
                .map_err(|err| err.to_string())?
                .unwrap_or_default(),
            seconds_ago: module.seconds_ago,
        })
    }

    /// Applies a delta's attribute updates.
    ///
    /// A delta carrying the `dynamic_fee_module` marker fully re-initializes the config; markerless
    /// deltas apply only the dynamic-fee fields they contain, leaving the rest untouched.
    pub(crate) fn update_from_attributes(
        &mut self,
        attributes: &HashMap<String, Bytes>,
    ) -> Result<(), String> {
        if attributes.contains_key(DYNAMIC_FEE_MODULE_ATTRIBUTE) {
            *self = Self::from_attributes(attributes)?;
            return Ok(());
        }

        if let Some(base_fee) = attributes.get("dfc_baseFee") {
            self.base_fee = u32::try_from(base_fee.clone()).map_err(|err| err.to_string())?;
        }
        if let Some(fee_cap) = attributes.get("dfc_feeCap") {
            self.fee_cap = u32::try_from(fee_cap.clone()).map_err(|err| err.to_string())?;
        }
        if let Some(scaling_factor) = attributes.get("dfc_scalingFactor") {
            self.scaling_factor =
                u64::try_from(scaling_factor.clone()).map_err(|err| err.to_string())?;
        }
        if let Some(initial_fee_enabled) = attributes.get("dfc_initialFeeEnabled") {
            self.initial_fee_enabled = initial_fee_enabled
                .iter()
                .any(|byte| *byte != 0);
        }
        if let Some(initial_fee) = attributes.get("dfc_initialFee") {
            self.initial_fee = u32::try_from(initial_fee.clone()).map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    pub fn scaling_factor(&self) -> u64 {
        self.scaling_factor
    }
}

/// A resolved swap fee, in pips, plus whether resolving it required a TWAP `observe`.
///
/// Under the first-in-block assumption the module short-circuits to the initial fee before
/// touching the oracle, so such a swap does not pay for the observation reads either. Under the
/// worst-case default the oracle read is charged whenever the dynamic branch evaluated it, even
/// when the initial fee wins the max — conservative on both fee and gas.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFee {
    pub(crate) fee: u32,
    pub(crate) observed_twap: bool,
}

impl ResolvedFee {
    fn flat(fee: u32) -> Self {
        Self { fee, observed_twap: false }
    }
}

/// Resolves the fee a swap executing at `execution_timestamp` would pay.
///
/// When no observation has been written in the execution block yet, the swap's position within
/// the block is unknown: `assume_first_in_block` quotes the initial fee (betting the swap lands
/// before any other on the pool), while the default quotes the worse of the initial and dynamic
/// fees so the output is never over-quoted. Once the pool has been touched in the execution
/// block, position is a known fact and the flag has no effect.
#[expect(
    clippy::too_many_arguments,
    reason = "inputs mirror the on-chain dynamic fee module and pool state"
)]
pub fn get_dynamic_fee(
    dfc: &DynamicFeeConfig,
    default_base_fee: u32,
    current_tick: i32,
    liquidity: u128,
    observation_index: u16,
    observation_cardinality: u16,
    observations: &Observations,
    execution_timestamp: u32,
    assume_first_in_block: bool,
) -> Result<ResolvedFee, SimulationError> {
    if dfc.base_fee == ZERO_FEE_INDICATOR {
        return Ok(ResolvedFee::flat(0));
    }
    let base_fee = if dfc.base_fee != 0 { dfc.base_fee } else { default_base_fee };

    if dfc.initial_fee_enabled &&
        observations.timestamp_at(observation_index, observation_cardinality)? !=
            execution_timestamp
    {
        let initial = match dfc.initial_fee {
            0 => base_fee,
            ZERO_FEE_INDICATOR => 0,
            initial_fee => initial_fee,
        };
        if assume_first_in_block {
            // First in block: the module returns before touching the oracle.
            return Ok(ResolvedFee::flat(initial));
        }
        // Position unknown: quote the worse of the two branches, since nothing stops a pool from
        // configuring `initialFee` above its dynamic fee. Gas stays conservative too: the oracle
        // read is charged whenever the dynamic branch evaluated it.
        let dynamic = resolve_dynamic_fee(
            dfc,
            base_fee,
            current_tick,
            liquidity,
            observation_index,
            observation_cardinality,
            observations,
            execution_timestamp,
        )?;
        return Ok(ResolvedFee { fee: initial.max(dynamic.fee), ..dynamic });
    }

    resolve_dynamic_fee(
        dfc,
        base_fee,
        current_tick,
        liquidity,
        observation_index,
        observation_cardinality,
        observations,
        execution_timestamp,
    )
}

/// The dynamic branch of the module: base fee plus the TWAP-deviation component, capped.
#[expect(
    clippy::too_many_arguments,
    reason = "inputs mirror the on-chain dynamic fee module and pool state"
)]
fn resolve_dynamic_fee(
    dfc: &DynamicFeeConfig,
    base_fee: u32,
    current_tick: i32,
    liquidity: u128,
    observation_index: u16,
    observation_cardinality: u16,
    observations: &Observations,
    execution_timestamp: u32,
) -> Result<ResolvedFee, SimulationError> {
    let (scaling_factor, fee_cap) = if dfc.scaling_factor != 0 {
        (dfc.scaling_factor, dfc.fee_cap)
    } else {
        (DEFAULT_SCALING_FACTOR, DEFAULT_FEE_CAP)
    };
    // The module runs the TWAP computation for any scaling factor — a zero factor just
    // multiplies the deviation away — so the observe gas is paid whenever the cardinality guard
    // passes. Verified with debug_traceCall of fee() on a zero-scaling pool: the observe subcall
    // burns ~50k gas.
    let (dynamic_component, observed_twap) = calculate_dynamic_fee(
        current_tick,
        liquidity,
        observation_index,
        observation_cardinality,
        observations,
        execution_timestamp,
        scaling_factor,
        dfc.seconds_ago,
    )?;
    Ok(ResolvedFee { fee: (base_fee + dynamic_component).min(fee_cap), observed_twap })
}

#[expect(
    clippy::too_many_arguments,
    reason = "inputs mirror the on-chain dynamic fee module and pool state"
)]
fn calculate_dynamic_fee(
    current_tick: i32,
    liquidity: u128,
    observation_index: u16,
    observation_cardinality: u16,
    observations: &Observations,
    blocktime: u32,
    scaling_factor: u64,
    seconds_ago: u32,
) -> Result<(u32, bool), SimulationError> {
    if u32::from(observation_cardinality) < seconds_ago / MIN_SECONDS_AGO {
        // The module returns before reaching the oracle: no observe gas.
        return Ok((0, false));
    }
    // The module wraps `observe` in try/catch and adds no dynamic component when it reverts,
    // which the oracle does with `OLD` when the window reaches past the oldest observation. Any
    // other failure is missing or inconsistent data in this copy of the oracle, which the chain
    // never sees, so the quote fails rather than drop a fee the pool charges.
    let (tick_cumulatives, _) = match observations.observe(
        blocktime,
        &[seconds_ago, 0],
        current_tick,
        observation_index,
        liquidity,
        observation_cardinality,
    ) {
        Ok(observed) => observed,
        Err(ObserveError::TooOld) => return Ok((0, true)),
        Err(ObserveError::Data(err)) => return Err(err),
    };
    let average = (tick_cumulatives[1] - tick_cumulatives[0]) / i64::from(seconds_ago);
    let tw_avg_tick = i32::try_from(average).map_err(|e| {
        SimulationError::FatalError(format!("Time-weighted average tick {average}: {e}"))
    })?;

    let abs_tick_delta = (current_tick - tw_avg_tick).unsigned_abs();

    let dynamic_fee = (u128::from(abs_tick_delta) * u128::from(scaling_factor)) / SCALING_PRECISION;
    let dynamic_fee = u32::try_from(dynamic_fee).map_err(|e| {
        SimulationError::FatalError(format!("Dynamic fee component {dynamic_fee}: {e}"))
    })?;
    Ok((dynamic_fee, true))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;
    use crate::evm::protocol::utils::slipstreams::observations::Observation;

    #[test]
    fn returns_initial_fee_before_first_observation_in_block() {
        let dfc = DynamicFeeConfig::new(150, 400, 6_000_000, true, 30);
        let observations = Observations::new(vec![Observation {
            block_timestamp: 99,
            initialized: true,
            index: 0,
            ..Default::default()
        }]);

        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 100, true)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, ResolvedFee::flat(30));
    }

    #[test]
    fn uses_dynamic_fee_after_observation_is_written_in_block() {
        let dfc = DynamicFeeConfig::new(150, 400, 0, true, 30);
        let observations = Observations::new(vec![Observation {
            block_timestamp: 100,
            initialized: true,
            index: 0,
            ..Default::default()
        }]);

        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 100, true)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, ResolvedFee::flat(150));
    }

    #[rstest]
    #[case::base_fee_fallback(0, 150)]
    #[case::zero_fee_indicator(ZERO_FEE_INDICATOR, 0)]
    fn interprets_initial_fee_sentinel_values(#[case] initial_fee: u32, #[case] expected_fee: u32) {
        let observations = Observations::new(vec![Observation {
            block_timestamp: 99,
            initialized: true,
            index: 0,
            ..Default::default()
        }]);

        let dfc = DynamicFeeConfig::new(150, 400, 0, true, initial_fee);
        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 100, true)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee.fee, expected_fee);
    }

    #[test]
    fn test_get_dynamic_fee() {
        let dfc = DynamicFeeConfig::new(350, 550, 3000000, false, 0);

        let items: &[(i32, [u8; 32])] = &[
            (534, hex!("01000006a0000001485073e3b9c1b1ba599e933717fff778eb3ff056690b05a1")),
            (535, hex!("01000006a0000001485072f3c785d8212da7414577fff77952e1b5ec690ae2d7")),
            (2039, hex!("01000006a00000014850737217b73678a71c3a98fdfff7791d60d77e690af4cd")),
            (2040, hex!("01000006a00000014850737239d75dce70d02120fffff7791d5ae25e690af4cf")),
            (2792, hex!("01000006a0000001485073a83aca03f518567293a7fff779047fd580690afd27")),
            (2793, hex!("01000006a0000001485073a84425ed86707cf3426afff7790473eae0690afd2b")),
            (158, hex!("01000006a0000001485073c3bd34f1a8b1e3595ef2fff778f7fab090690b015b")),
            (159, hex!("01000006a0000001485073c3bfcaf719d5a10d1babfff778f7f4bb8c690b015d")),
            (346, hex!("01000006a0000001485073d0992e1120d6162dec08fff778f1d01820690b036d")),
            (347, hex!("01000006a0000001485073d0a6e39cdb5daeb0b3dcfff778f1be390e690b0373")),
            (252, hex!("01000006a0000001485073ca9b84bedd861255f8f6fff778f4ca8f18690b026d")),
            (253, hex!("01000006a0000001485073caa9f9acbba60b0ac592fff778f4c49a22690b026f")),
            (299, hex!("01000006a0000001485073cc668d277afc9908bf5afff778f35f3120690b02e7")),
            (300, hex!("01000006a0000001485073cc6cec51a251dcd597a7fff778f3593c34690b02e9")),
            (322, hex!("01000006a0000001485073ce6eb2883c6b9c109292fff778f2a688ec690b0325")),
            (323, hex!("01000006a0000001485073ce7c9a63260beda5874ffff778f2a09414690b0327")),
            (334, hex!("01000006a0000001485073cf5c7f6da6dee6f54694fff778f21d8926690b0353")),
            (335, hex!("01000006a0000001485073cf7b9869740c05f0bf44fff778f2179430690b0355")),
            (328, hex!("01000006a0000001485073cecedc4b2732bd3fb882fff778f24d3044690b0343")),
            (329, hex!("01000006a0000001485073ced7e92fc61bfda7ccd4fff778f2414694690b0347")),
            (331, hex!("01000006a0000001485073cee01b7e722a6b0763d3fff778f2355ce2690b034b")),
            (332, hex!("01000006a0000001485073ceff347a3f578a02dc83fff778f22f67f6690b034d")),
        ];
        let mut obs = Observations::new(vec![]);
        for (idx, bytes) in items.iter().copied() {
            assert!(obs
                .upsert_observation(idx, &bytes)
                .is_ok());
        }

        let dynamic_fee = get_dynamic_fee(
            &dfc,
            362,
            -195239,
            1102101691356476042,
            534,
            3010,
            &obs,
            1762330021,
            true,
        )
        .expect("Failed to calculate dynamic fee");

        assert_eq!(dynamic_fee, ResolvedFee { fee: 380, observed_twap: true });
    }

    #[test]
    fn applies_partial_config_updates_without_replacing_unchanged_fields() {
        let mut config = DynamicFeeConfig::new(100, 1_000, 5_000_000, true, 50);

        config
            .update_from_attributes(&HashMap::from([(
                "dfc_baseFee".to_string(),
                Bytes::from(200_u32.to_be_bytes()),
            )]))
            .expect("partial dynamic fee update should be valid");

        assert_eq!(config, DynamicFeeConfig::new(200, 1_000, 5_000_000, true, 50));
    }

    #[rstest]
    #[case::missing_module(None)]
    #[case::unsupported_module(Some(Bytes::from([0x11; 20])))]
    fn from_attributes_defaults_for_missing_or_unsupported_module(#[case] module: Option<Bytes>) {
        // Attributes are present but untrusted without a supported marker: fall back to default.
        let mut attributes = HashMap::from([
            ("dfc_baseFee".to_string(), Bytes::from(200_u32.to_be_bytes())),
            ("dfc_feeCap".to_string(), Bytes::from(1_000_u32.to_be_bytes())),
            ("dfc_scalingFactor".to_string(), Bytes::from(5_000_000_u64.to_be_bytes())),
        ]);
        if let Some(module) = module {
            attributes.insert(DYNAMIC_FEE_MODULE_ATTRIBUTE.to_string(), module);
        }

        assert_eq!(DynamicFeeConfig::from_attributes(&attributes), Ok(DynamicFeeConfig::default()));
    }

    #[test]
    fn default_config_falls_back_to_static_default_fee() {
        // base_fee == 0 must resolve to the pool's static default fee (3000 here).
        let dfc = DynamicFeeConfig::default();
        let observations = Observations::new(vec![Observation {
            block_timestamp: 100,
            initialized: true,
            index: 0,
            ..Default::default()
        }]);

        let fee = get_dynamic_fee(&dfc, 3000, 0, 1, 0, 1, &observations, 100, true)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, ResolvedFee::flat(3000));
    }

    /// A ring of `cardinality` slots holding the given `(index, timestamp, tick cumulative,
    /// seconds-per-liquidity cumulative)` observations; every other slot is unwritten.
    fn ring(cardinality: usize, written: &[(usize, u32, i64, &str)]) -> Observations {
        let mut slots = vec![Observation::default(); cardinality];
        for &(index, block_timestamp, tick_cumulative, seconds_per_liquidity) in written {
            slots[index] = Observation {
                block_timestamp,
                tick_cumulative,
                seconds_per_liquidity_cumulative_x128: U256::from_str_radix(
                    seconds_per_liquidity,
                    10,
                )
                .expect("decimal cumulative"),
                initialized: true,
                index: i32::try_from(index).expect("slot index fits i32"),
            };
        }
        Observations::new(slots)
    }

    #[test]
    fn observe_reverting_old_charges_no_dynamic_fee() {
        // The only observation is younger than the 600 s window, so `observe` reverts `OLD`. The
        // module catches it: base fee only, and the attempted observe still costs gas.
        let dfc = DynamicFeeConfig::new(0, 10_000, 1_000_000, false, 0);
        let observations = ring(300, &[(0, 999_500, 0, "0")]);

        let fee = get_dynamic_fee(&dfc, 3000, 7, 1, 0, 300, &observations, 1_000_000, true)
            .expect("an OLD revert falls back to the base fee");

        assert_eq!(fee, ResolvedFee { fee: 3000, observed_twap: true });
    }

    #[test]
    fn observation_data_error_fails_the_quote() {
        // Slot 5 is the pool's current index but holds nothing: the chain cannot be in this
        // state, so it is missing data here, not an `OLD` revert to absorb.
        let dfc = DynamicFeeConfig::new(0, 10_000, 1_000_000, false, 0);
        let observations = Observations::new(vec![Observation {
            block_timestamp: 999_000,
            initialized: true,
            index: 0,
            ..Default::default()
        }]);

        let result = get_dynamic_fee(&dfc, 3000, 7, 1, 5, 300, &observations, 1_000_000, true);

        assert!(result.is_err(), "got {result:?}");
    }

    #[test]
    fn young_ring_reads_unwritten_slots_as_uninitialized() {
        // Robinhood pool 0x8132252bd65e7c03f7b57739e1db1c58d6ee792f at block 61,011,933: slots
        // 0-2 written, cardinality 500, the rest prefilled but unwritten. Config (50, 500, 1e7)
        // under a 600 s module; `getFee` returns 110 on chain, 50 base plus 6 ticks of TWAP
        // deviation at 1e7 / 1e6.
        let dfc = DynamicFeeConfig::new(50, 500, 10_000_000, false, 0);
        let observations = ring(
            500,
            &[
                (0, 1_789_205_466, -3_753_782_792, "4871078707023353428616415205"),
                (1, 1_789_206_050, -3_869_381_504, "4943578347386704671325331035"),
                (2, 1_789_206_560, -3_970_336_004, "5006891389484836749718391092"),
            ],
        );

        let fee = get_dynamic_fee(
            &dfc,
            500,
            -197_954,
            18_176_761_252_316_136,
            2,
            500,
            &observations,
            1_789_206_560,
            true,
        )
        .expect("fee");

        assert_eq!(fee, ResolvedFee { fee: 110, observed_twap: true });
    }

    #[test]
    fn twap_window_is_the_modules_seconds_ago() {
        // Base pool 0x3fe04a59ebd38cf06080a6f60a98d124eb59392a at block 53,246,841 (ts
        // 1,793,283,029), on module 0x87D8 whose `secondsAgo` is 30: config (250, 10_000, 1.25e8),
        // tick -198,816, index 35 of 1,801. The slots are the ones both windows' binary searches
        // read on chain. `getFee` returns 625: a 3-tick deviation over 30 s. A 600 s window
        // deviates by 9 ticks and would charge 1,375.
        let attributes = HashMap::from([
            (
                DYNAMIC_FEE_MODULE_ATTRIBUTE.to_string(),
                Bytes::from(hex!("87D8f999BBa9343E8099552426775B51C338E8CB")),
            ),
            ("dfc_baseFee".to_string(), Bytes::from(250_u32.to_be_bytes())),
            ("dfc_feeCap".to_string(), Bytes::from(10_000_u32.to_be_bytes())),
            ("dfc_scalingFactor".to_string(), Bytes::from(125_000_000_u64.to_be_bytes())),
        ]);
        let dfc = DynamicFeeConfig::from_attributes(&attributes).expect("supported module");
        let observations = ring(
            1801,
            &[
                (0, 1793282575, -3275662076386, "4825203962939739569257789745571497048595234"),
                (7, 1793282623, -3275671619302, "4825203962939739569276893739575444000335090"),
                (8, 1793282633, -3275673607392, "4825203962939739569280873738326266281947560"),
                (21, 1793282821, -3275710982996, "4825203962939739569355697714841725176262000"),
                (22, 1793282845, -3275715754508, "4825203962939739569365249711843698652131929"),
                (28, 1793282925, -3275731659364, "4825203962939739569397089714845879783423417"),
                (29, 1793282931, -3275732852212, "4825203962939739569399477720594174591586762"),
                (30, 1793282943, -3275735237920, "4825203962939739569404253732090764207913453"),
                (31, 1793282975, -3275741599840, "4825203962939739569416989762748336518117962"),
                (32, 1793283009, -3275748359448, "4825203962939739569430521795322007097710253"),
                (33, 1793283013, -3275749154700, "4825203962939739569432113799154203636485816"),
                (35, 1793283025, -3275751540474, "4825203962939739569436889810650793252812506"),
                (36, 1793244467, -3268084691080, "4825203962939739551051757126085317697368620"),
                (936, 1793265129, -3272193430366, "4825203962939739562297101138547303077297501"),
                (937, 1793265139, -3272195418466, "4825203962939739562301084452370714260727039"),
                (1386, 1793276999, -3274553455060, "4825203962939739567024672774306500132937332"),
                (1387, 1793277003, -3274554250332, "4825203962939739567026265905404686576486673"),
                (1611, 1793280663, -3275281921370, "4825203962939739568483314884286832984753085"),
                (1612, 1793280665, -3275282318968, "4825203962939739568484112377091106362391932"),
                (1724, 1793281749, -3275497853804, "4825203962939739568928959549000027675629456"),
                (1725, 1793281759, -3275499842184, "4825203962939739568932939571799223800243474"),
                (1780, 1793282351, -3275617544942, "4825203962939739569168558897552147796963386"),
                (1781, 1793282359, -3275619135318, "4825203962939739569171748849458536199122231"),
                (1787, 1793282393, -3275625894320, "4825203962939739569185306145060686908297322"),
                (1788, 1793282399, -3275627087114, "4825203962939739569187698608990478209916456"),
                (1790, 1793282417, -3275630665516, "4825203962939739569194874512553419470556641"),
                (1791, 1793282427, -3275632653536, "4825203962939739569198854511304241752169111"),
                (1792, 1793282457, -3275638617626, "4825203962939739569210794507556708597006522"),
                (1793, 1793282459, -3275639015230, "4825203962939739569211590507306873053329016"),
                (1794, 1793282485, -3275644184004, "4825203962939739569221957851002635360345264"),
                (1795, 1793282501, -3275647364804, "4825203962939739569228337754815412164662955"),
            ],
        );
        let fee_over = |dfc: &DynamicFeeConfig| {
            get_dynamic_fee(
                dfc,
                3000,
                -198_816,
                854_978_763_339_884_583,
                35,
                1801,
                &observations,
                1_793_283_029,
                true,
            )
            .expect("fee")
            .fee
        };

        assert_eq!(fee_over(&dfc), 625);
        assert_eq!(fee_over(&dfc.with_seconds_ago(DEFAULT_SECONDS_AGO)), 1375);
    }

    #[test]
    fn zero_scaling_still_pays_for_the_observe() {
        // A zero scaling factor only multiplies the deviation away; the oracle is still read, so
        // the observe gas is still paid. Only the cardinality guard skips the oracle.
        let dfc = DynamicFeeConfig::new(2700, 0, 0, false, 0);
        let observations = Observations::new(vec![
            Observation {
                block_timestamp: 999_000,
                initialized: true,
                index: 0,
                ..Default::default()
            },
            Observation {
                block_timestamp: 999_500,
                initialized: true,
                index: 1,
                ..Default::default()
            },
        ]);

        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 1, 300, &observations, 1_000_000, true)
            .expect("fee should be computable");
        assert_eq!(fee, ResolvedFee { fee: 2700, observed_twap: true });

        let guarded = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 1_000_000, true)
            .expect("fee should be computable");
        assert_eq!(guarded, ResolvedFee::flat(2700), "cardinality guard skips the oracle");
    }
}
