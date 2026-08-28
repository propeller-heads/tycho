use std::collections::HashMap;

use alloy::primitives::{address, Address};
use serde::{Deserialize, Serialize};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::protocol::utils::slipstreams::observations::Observations;

pub(crate) const ZERO_FEE_INDICATOR: u32 = 420;
pub(crate) const DEFAULT_SECONDS_AGO: u32 = 600;
pub(crate) const MIN_SECONDS_AGO: u32 = 2;
pub(crate) const DEFAULT_SCALING_FACTOR: u64 = 0;
// The upgraded DynamicSwapFeeModule deployments use 30_000 as their default fee cap.
pub(crate) const DEFAULT_FEE_CAP: u32 = 30_000;
const SCALING_PRECISION: u128 = 1_000_000;
const SUPPORTED_DYNAMIC_FEE_MODULES: [Address; 3] = [
    address!("090b2A6bb475c00e2256e2095A60887cD710803b"),
    address!("F4Ecd78EBEB6d36CF7f80B5B6B41453515fe2785"),
    address!("87D8f999BBa9343E8099552426775B51C338E8CB"),
];
const DYNAMIC_FEE_MODULE_ATTRIBUTE: &str = "dynamic_fee_module";

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicFeeConfig {
    base_fee: u32,
    fee_cap: u32,
    scaling_factor: u64,
    initial_fee_enabled: bool,
    initial_fee: u32,
}

fn is_supported_dynamic_fee_module(fee_module: &[u8]) -> bool {
    SUPPORTED_DYNAMIC_FEE_MODULES
        .iter()
        .any(|supported| supported.as_slice() == fee_module)
}

impl DynamicFeeConfig {
    pub fn new(
        base_fee: u32,
        fee_cap: u32,
        scaling_factor: u64,
        initial_fee_enabled: bool,
        initial_fee: u32,
    ) -> Self {
        Self { base_fee, fee_cap, scaling_factor, initial_fee_enabled, initial_fee }
    }

    /// Builds a config from a full snapshot's attributes.
    ///
    /// An absent or unsupported `dynamic_fee_module` marker yields the default config, whose
    /// `base_fee == 0` makes `get_dynamic_fee` fall back to the pool's static `default_fee`. Only
    /// errors when a supported marker is present but a dynamic-fee attribute is missing.
    pub(crate) fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, &'static str> {
        if !attributes
            .get(DYNAMIC_FEE_MODULE_ATTRIBUTE)
            .is_some_and(|module| is_supported_dynamic_fee_module(module))
        {
            return Ok(Self::default());
        }

        Ok(Self {
            base_fee: u32::from(
                attributes
                    .get("dfc_baseFee")
                    .ok_or("missing dynamic fee attribute `dfc_baseFee`")?
                    .clone(),
            ),
            fee_cap: u32::from(
                attributes
                    .get("dfc_feeCap")
                    .ok_or("missing dynamic fee attribute `dfc_feeCap`")?
                    .clone(),
            ),
            scaling_factor: u64::from(
                attributes
                    .get("dfc_scalingFactor")
                    .ok_or("missing dynamic fee attribute `dfc_scalingFactor`")?
                    .clone(),
            ),
            initial_fee_enabled: attributes
                .get("dfc_initialFeeEnabled")
                .is_some_and(|value| value.iter().any(|byte| *byte != 0)),
            initial_fee: attributes
                .get("dfc_initialFee")
                .cloned()
                .map(u32::from)
                .unwrap_or_default(),
        })
    }

    /// Applies a delta's attribute updates.
    ///
    /// A delta carrying the `dynamic_fee_module` marker fully re-initializes the config; markerless
    /// deltas apply only the dynamic-fee fields they contain, leaving the rest untouched.
    pub(crate) fn update_from_attributes(
        &mut self,
        attributes: &HashMap<String, Bytes>,
    ) -> Result<(), &'static str> {
        if attributes.contains_key(DYNAMIC_FEE_MODULE_ATTRIBUTE) {
            *self = Self::from_attributes(attributes)?;
            return Ok(());
        }

        if let Some(base_fee) = attributes.get("dfc_baseFee") {
            self.base_fee = u32::from(base_fee.clone());
        }
        if let Some(fee_cap) = attributes.get("dfc_feeCap") {
            self.fee_cap = u32::from(fee_cap.clone());
        }
        if let Some(scaling_factor) = attributes.get("dfc_scalingFactor") {
            self.scaling_factor = u64::from(scaling_factor.clone());
        }
        if let Some(initial_fee_enabled) = attributes.get("dfc_initialFeeEnabled") {
            self.initial_fee_enabled = initial_fee_enabled
                .iter()
                .any(|byte| *byte != 0);
        }
        if let Some(initial_fee) = attributes.get("dfc_initialFee") {
            self.initial_fee = u32::from(initial_fee.clone());
        }
        Ok(())
    }

    pub fn scaling_factor(&self) -> u64 {
        self.scaling_factor
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn get_dynamic_fee(
    dfc: &DynamicFeeConfig,
    default_base_fee: u32,
    current_tick: i32,
    liquidity: u128,
    observation_index: u16,
    observation_cardinality: u16,
    observations: &Observations,
    blocktime: u32,
) -> Result<u32, SimulationError> {
    if dfc.base_fee == ZERO_FEE_INDICATOR {
        return Ok(0);
    }
    let base_fee = if dfc.base_fee != 0 { dfc.base_fee } else { default_base_fee };

    if dfc.initial_fee_enabled &&
        observations.timestamp_at(observation_index, observation_cardinality)? != blocktime
    {
        return match dfc.initial_fee {
            0 => Ok(base_fee),
            ZERO_FEE_INDICATOR => Ok(0),
            initial_fee => Ok(initial_fee),
        };
    }

    let (scaling_factor, fee_cap) = if dfc.scaling_factor != 0 {
        (dfc.scaling_factor, dfc.fee_cap)
    } else {
        (DEFAULT_SCALING_FACTOR, DEFAULT_FEE_CAP)
    };
    let total_fee = base_fee +
        calculate_dynamic_fee(
            current_tick,
            liquidity,
            observation_index,
            observation_cardinality,
            observations,
            blocktime,
            scaling_factor,
        )?;
    Ok(total_fee.min(fee_cap))
}

fn calculate_dynamic_fee(
    current_tick: i32,
    liquidity: u128,
    observation_index: u16,
    observation_cardinality: u16,
    observations: &Observations,
    blocktime: u32,
    scaling_factor: u64,
) -> Result<u32, SimulationError> {
    if observation_cardinality < (DEFAULT_SECONDS_AGO / MIN_SECONDS_AGO) as u16 {
        return Ok(0);
    };
    // Mirror the on-chain module's `try pool.observe(...) { ... } catch { return 0; }`: a failed
    // observation (e.g. a ring buffer Tycho only partially indexed) contributes no dynamic
    // component instead of failing the whole quote.
    let observed = match observations.observe(
        blocktime,
        &[DEFAULT_SECONDS_AGO, 0],
        current_tick,
        observation_index,
        liquidity,
        observation_cardinality,
    ) {
        Ok(observed) => observed,
        Err(_) => return Ok(0),
    };
    let tw_avg_tick = match observed {
        (tick_cumulatives, _) if tick_cumulatives.len() >= 2 => {
            ((tick_cumulatives[1] - tick_cumulatives[0]) / DEFAULT_SECONDS_AGO as i64) as i32
        }
        _ => return Ok(0),
    };

    let abs_tick_delta = (current_tick - tw_avg_tick).unsigned_abs();

    let dynamic_fee = (abs_tick_delta as u128 * scaling_factor as u128) / SCALING_PRECISION;
    Ok(dynamic_fee as u32)
}

#[cfg(test)]
mod tests {
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

        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 100)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, 30);
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

        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 100)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, 150);
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
        let fee = get_dynamic_fee(&dfc, 500, 0, 1, 0, 1, &observations, 100)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, expected_fee);
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

        let dynamic_fee =
            get_dynamic_fee(&dfc, 362, -195239, 1102101691356476042, 534, 3010, &obs, 1762330021)
                .expect("Failed to calculate dynamic fee");

        assert_eq!(dynamic_fee, 380);
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

        let fee = get_dynamic_fee(&dfc, 3000, 0, 1, 0, 1, &observations, 100)
            .expect("Failed to calculate dynamic fee");

        assert_eq!(fee, 3000);
    }

    #[test]
    fn observe_failure_falls_back_to_base_fee() {
        // cardinality clears the min-history guard but exceeds the indexed buffer, so observe()
        // errors out of bounds. Like the on-chain try/catch, the dynamic component is 0 and the
        // fee falls back to the base fee, instead of failing the quote.
        let dfc = DynamicFeeConfig::default();
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

        let fee = get_dynamic_fee(&dfc, 3000, 0, 1, 1, 300, &observations, 1_000_000)
            .expect("observe failure should fall back to the base fee, not error");

        assert_eq!(fee, 3000);
    }
}
