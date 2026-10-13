//! Port of Algebra V1.9's `AdaptiveFee` library and of
//! `DataStorageOperator.calculateVolumePerLiquidity`, as deployed by Camelot V3 on Arbitrum One.
//!
//! Solidity 0.7.6 arithmetic is unchecked. Every intermediate value here is bounded well below
//! 2^256 by the contract's own invariants (documented inline), so plain `U256` operations are
//! exact; the two places where Solidity narrows a value (`uint16(fee)` and `uint128(volume)`)
//! are reproduced explicitly.

use alloy::primitives::{I256, U256};
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::sqrt_u256;

/// `100000 << 64`: the largest volume-to-liquidity ratio the operator records per swap.
const MAX_VOLUME_PER_LIQUIDITY: u128 = 100_000u128 << 64;

/// `AdaptiveFee.Configuration`: the sigmoid coefficients of one swap direction.
///
/// The fee is `baseFee + sigmoid_volume(sigmoid_1(volatility) + sigmoid_2(volatility))`, where
/// each sigmoid is `alpha / (1 + e^((beta - x) / gamma))`. The factory writes a configuration
/// into every new pool's `DataStorageOperator` and its owner can change it later, so the
/// values are pool state rather than constants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeConfiguration {
    pub alpha1: u16,
    pub alpha2: u16,
    pub beta1: u32,
    pub beta2: u32,
    pub gamma1: u16,
    pub gamma2: u16,
    pub volume_beta: u32,
    pub volume_gamma: u16,
    pub base_fee: u16,
}

impl FeeConfiguration {
    /// Decodes the storage slot that holds a configuration.
    ///
    /// Solidity packs the nine fields into the low 24 bytes of the slot in declaration order,
    /// starting from the low end. The attribute may arrive with its leading zero bytes
    /// stripped, so any length up to 32 bytes is accepted.
    pub fn from_slot(bytes: &[u8]) -> Result<Self, SimulationError> {
        if bytes.len() > 32 {
            return Err(SimulationError::FatalError(format!(
                "fee configuration of {} bytes is longer than a storage slot",
                bytes.len()
            )));
        }
        let mut slot = [0u8; 32];
        slot[32 - bytes.len()..].copy_from_slice(bytes);
        let mut reader = LowEndReader::new(&slot);
        Ok(Self {
            alpha1: reader.u16(),
            alpha2: reader.u16(),
            beta1: reader.u32(),
            beta2: reader.u32(),
            gamma1: reader.u16(),
            gamma2: reader.u16(),
            volume_beta: reader.u32(),
            volume_gamma: reader.u16(),
            base_fee: reader.u16(),
        })
    }

    /// `AdaptiveFee.getFee`: the fee in hundredths of a bip for the given 1-day averages.
    ///
    /// `volatility` is the operator's average volatility already divided by 15, as
    /// `DataStorageOperator.getFees` passes it. Errors only when a gamma is zero, which the
    /// operator rejects at configuration time.
    pub fn get_fee(
        &self,
        volatility: U256,
        volume_per_liquidity: U256,
    ) -> Result<u16, SimulationError> {
        let mut sum_of_sigmoids =
            sigmoid(volatility, self.gamma1, self.alpha1, U256::from(self.beta1))? +
                sigmoid(volatility, self.gamma2, self.alpha2, U256::from(self.beta2))?;
        if sum_of_sigmoids > U256::from(u16::MAX) {
            sum_of_sigmoids = U256::from(u16::MAX);
        }
        let fee = U256::from(self.base_fee) +
            sigmoid(
                volume_per_liquidity,
                self.volume_gamma,
                sum_of_sigmoids.to::<u16>(),
                U256::from(self.volume_beta),
            )?;
        // `uint16(...)`: a no-op under the operator's `alpha1 + alpha2 + baseFee <= uint16.max`
        // invariant, kept so that the result is always what the contract would store.
        Ok(fee.to::<u64>() as u16)
    }
}

/// `AdaptiveFee.sigmoid`: `alpha / (1 + e^((beta - x) / g))`, never above `alpha`.
fn sigmoid(x: U256, g: u16, alpha: u16, beta: U256) -> Result<U256, SimulationError> {
    if g == 0 {
        return Err(SimulationError::FatalError(
            "adaptive fee gamma is zero, which the operator rejects".to_string(),
        ));
    }
    let alpha = U256::from(alpha);
    // Beyond six gammas from the midpoint the sigmoid is saturated, and this bound keeps `x`
    // below 19 bits for the series below.
    let saturation = U256::from(6u64 * u64::from(g));
    let g8 = U256::from(g).pow(U256::from(8));
    if x > beta {
        let x = x - beta;
        if x >= saturation {
            return Ok(alpha);
        }
        let ex = exp(x, g, g8);
        Ok(alpha * ex / (g8 + ex))
    } else {
        let x = beta - x;
        if x >= saturation {
            return Ok(U256::ZERO);
        }
        let ex = g8 + exp(x, g, g8);
        Ok(alpha * g8 / ex)
    }
}

/// `AdaptiveFee.exp`: `e^(x / g) * g^8` as the Taylor series up to the eighth power.
///
/// With `x < 6g < 2^19` and `g < 2^16` every term stays below 2^152 and the sum below 2^155.
fn exp(x: U256, g: u16, g_highest_degree: U256) -> U256 {
    let g = U256::from(g);
    let mut x_lowest_degree = x;
    let mut res = g_highest_degree; // g^8
    let mut g_highest_degree = g_highest_degree / g; // g^7
    res += x_lowest_degree * g_highest_degree;

    g_highest_degree /= g; // g^6
    x_lowest_degree *= x; // x^2
    res += x_lowest_degree * g_highest_degree / U256::from(2);

    g_highest_degree /= g; // g^5
    x_lowest_degree *= x; // x^3
    res += x_lowest_degree * g_highest_degree / U256::from(6);

    g_highest_degree /= g; // g^4
    x_lowest_degree *= x; // x^4
    res += x_lowest_degree * g_highest_degree / U256::from(24);

    g_highest_degree /= g; // g^3
    x_lowest_degree *= x; // x^5
    res += x_lowest_degree * g_highest_degree / U256::from(120);

    g_highest_degree /= g; // g^2
    x_lowest_degree *= x; // x^6
    res += x_lowest_degree * g_highest_degree / U256::from(720);

    x_lowest_degree *= x; // x^7
    res += x_lowest_degree * g / U256::from(5040) + x_lowest_degree * x / U256::from(40320);
    res
}

/// `DataStorageOperator.calculateVolumePerLiquidity`: the swap's geometric-mean volume per
/// unit of liquidity, as the pool adds it to `volumePerLiquidityInBlock` after every swap.
///
/// `amount0` and `amount1` are the pool's signed token deltas; only their magnitudes matter.
pub fn calculate_volume_per_liquidity(
    liquidity: u128,
    amount0: I256,
    amount1: I256,
) -> Result<u128, SimulationError> {
    // Each root is below 2^128, so the product fits.
    let volume = sqrt_u256(amount0.unsigned_abs())? * sqrt_u256(amount1.unsigned_abs())?;
    let divisor = U256::from(liquidity.max(1));
    let volume_shifted =
        if volume >= U256::from(1) << 192 { U256::MAX / divisor } else { (volume << 64) / divisor };
    let cap = U256::from(MAX_VOLUME_PER_LIQUIDITY);
    Ok(if volume_shifted >= cap { MAX_VOLUME_PER_LIQUIDITY } else { volume_shifted.to::<u128>() })
}

/// Reads fixed-width big-endian fields out of a storage word, lowest bytes first, the way
/// Solidity lays out packed struct members.
struct LowEndReader<'a> {
    slot: &'a [u8; 32],
    end: usize,
}

impl<'a> LowEndReader<'a> {
    fn new(slot: &'a [u8; 32]) -> Self {
        Self { slot, end: 32 }
    }

    fn take(&mut self, len: usize) -> &'a [u8] {
        let start = self.end - len;
        let bytes = &self.slot[start..self.end];
        self.end = start;
        bytes
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(
            self.take(2)
                .try_into()
                .expect("two bytes"),
        )
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(
            self.take(4)
                .try_into()
                .expect("four bytes"),
        )
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// `feeConfigZto` of the WETH/USDC pool `0xB1026b8e…7526` at Arbitrum block 512282036,
    /// storage slot 131072 of its operator, verified against the `feeConfigZto()` getter.
    const WETH_USDC_CONFIG_SLOT: [u8; 32] =
        hex_literal::hex!("00000000000000000064000a000000002134003b0000ea60000002d002580000");

    fn weth_usdc_config() -> FeeConfiguration {
        FeeConfiguration {
            alpha1: 0,
            alpha2: 600,
            beta1: 720,
            beta2: 60_000,
            gamma1: 59,
            gamma2: 8_500,
            volume_beta: 0,
            volume_gamma: 10,
            base_fee: 100,
        }
    }

    #[test]
    fn decodes_the_packed_configuration_slot() {
        assert_eq!(
            FeeConfiguration::from_slot(&WETH_USDC_CONFIG_SLOT).unwrap(),
            weth_usdc_config()
        );
    }

    #[test]
    fn decodes_a_slot_with_leading_zeros_stripped() {
        let stripped = &WETH_USDC_CONFIG_SLOT[8..];
        assert_eq!(FeeConfiguration::from_slot(stripped).unwrap(), weth_usdc_config());
    }

    #[test]
    fn rejects_an_oversized_slot() {
        assert!(FeeConfiguration::from_slot(&[0u8; 33]).is_err());
    }

    #[test]
    fn fee_is_the_base_fee_without_volatility_or_volume() {
        // Both sigmoids sit far left of their midpoints and the volume sigmoid of a zero sum is
        // zero, so only the base fee remains.
        assert_eq!(
            weth_usdc_config()
                .get_fee(U256::ZERO, U256::ZERO)
                .unwrap(),
            100
        );
    }

    #[test]
    fn fee_saturates_at_base_plus_alphas() {
        let config = weth_usdc_config();
        let fee = config
            .get_fee(U256::from(u64::MAX), U256::from(u64::MAX))
            .unwrap();
        assert_eq!(fee, 100 + 600);
    }

    #[test]
    fn fee_never_exceeds_the_saturated_value_and_grows_with_volatility() {
        let config = weth_usdc_config();
        let mut previous = 0;
        for volatility in [0u64, 100, 1_000, 10_000, 100_000, 1_000_000] {
            let fee = config
                .get_fee(U256::from(volatility), U256::from(u64::MAX))
                .unwrap();
            assert!(fee >= previous, "fee must not decrease with volatility");
            assert!(fee <= 700);
            previous = fee;
        }
    }

    #[test]
    fn sigmoid_is_half_alpha_at_its_midpoint() {
        // e^0 = 1, so alpha / (1 + 1); integer division rounds 300 / 2 exactly.
        assert_eq!(sigmoid(U256::from(720), 59, 600, U256::from(720)).unwrap(), U256::from(300));
    }

    #[test]
    fn zero_gamma_is_an_error() {
        assert!(sigmoid(U256::from(1), 0, 600, U256::ZERO).is_err());
    }

    #[rstest]
    #[case::zero_amounts(1_000, 0, 0, 0)]
    // sqrt(4) * sqrt(9) = 6; (6 << 64) / 3 = 2 << 64
    #[case::small_amounts(3, 4, -9, 2u128 << 64)]
    // Zero liquidity divides by one instead.
    #[case::zero_liquidity(0, 4, 9, 6u128 << 64)]
    fn volume_per_liquidity_matches_the_operator(
        #[case] liquidity: u128,
        #[case] amount0: i128,
        #[case] amount1: i128,
        #[case] expected: u128,
    ) {
        let result = calculate_volume_per_liquidity(
            liquidity,
            I256::try_from(amount0).unwrap(),
            I256::try_from(amount1).unwrap(),
        )
        .unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn volume_per_liquidity_is_capped() {
        let huge = I256::try_from(i128::MAX).unwrap();
        assert_eq!(
            calculate_volume_per_liquidity(1, huge, huge).unwrap(),
            MAX_VOLUME_PER_LIQUIDITY
        );
    }
}
