use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::utils::slipstreams::observations::Observations;

pub(crate) const ZERO_FEE_INDICATOR: u32 = 420;
pub(crate) const DEFAULT_SECONDS_AGO: u32 = 600;
pub(crate) const MIN_SECONDS_AGO: u32 = 2;
pub(crate) const DEFAULT_SCALING_FACTOR: u64 = 0;
pub(crate) const DEFAULT_FEE_CAP: u32 = 10000;
const SCALING_PRECISION: u128 = 1_000_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicFeeConfig {
    base_fee: u32,
    fee_cap: u32,
    scaling_factor: u64,
}

impl DynamicFeeConfig {
    pub fn new(base_fee: u32, fee_cap: u32, scaling_factor: u64) -> Self {
        Self { base_fee, fee_cap, scaling_factor }
    }
    pub fn update_fee_cap(&mut self, fee_cap: u32) {
        self.fee_cap = fee_cap;
    }
    pub fn update_scaling_factor(&mut self, scaling_factor: u64) {
        self.scaling_factor = scaling_factor;
    }
    pub fn update_base_fee(&mut self, base_fee: u32) {
        self.base_fee = base_fee;
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
    let tw_avg_tick = match observations.observe(
        blocktime,
        &[DEFAULT_SECONDS_AGO, 0],
        current_tick,
        observation_index,
        liquidity,
        observation_cardinality,
    )? {
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

    use super::*;
    #[test]
    fn test_get_dynamic_fee() {
        let dfc = DynamicFeeConfig::new(350, 550, 3000000);

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
}
