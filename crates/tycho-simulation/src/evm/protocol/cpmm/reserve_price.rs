use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use crate::{evm::protocol::u256_num::u256_to_f64, utils::decimals_exponent};

/// Computes a spot price given two token reserves
///
/// To find the most accurate spot price possible:
///     1. The nominator and denominator are converted to float (this conversion is lossy)
///     2. The price is computed by using float division
///     3. Finally, the price is correct for difference in token decimals.
///
/// An empty reserve has no price, and a price outside what an `f64` holds (decimals far apart)
/// is an error rather than infinity or zero.
pub(super) fn spot_price_from_reserves(
    r0: U256,
    r1: U256,
    token_0_decimals: u32,
    token_1_decimals: u32,
) -> Result<f64, SimulationError> {
    if r0.is_zero() || r1.is_zero() {
        return Err(SimulationError::RecoverableError("No liquidity".to_string()));
    }
    let token_correction = 10f64.powi(decimals_exponent(token_0_decimals, token_1_decimals)?);
    let price = (u256_to_f64(r1)? / u256_to_f64(r0)?) * token_correction;
    if !(price.is_finite() && price > 0.0) {
        return Err(SimulationError::InvalidInput(
            format!(
                "Spot price of reserves {r0} and {r1} at {token_0_decimals} and \
                 {token_1_decimals} decimals is {price}, outside what an f64 holds"
            ),
            None,
        ));
    }
    Ok(price)
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use approx::assert_ulps_eq;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::dai_weth(
        U256::from_str("6459290401503744160496018").unwrap(),
        U256::from_str("5271291858877575385159").unwrap(),
        18,
        18,
        0.0008160790940209781f64
    )]
    #[case::weth_usdt(
        U256::from_str("9404438958522240683671").unwrap(),
        U256::from_str("11524076256844").unwrap(),
        18,
        6,
        1225.3868952385467f64
    )]
    #[case::paxg_weth(
        U256::from_str("1953602660669219944829").unwrap(),
        U256::from_str("2875413366760000758700").unwrap(),
        18,
        18,
        1.4718516844029115f64
    )]
    fn test_real_world_examples(
        #[case] r0: U256,
        #[case] r1: U256,
        #[case] t0d: u32,
        #[case] t1d: u32,
        #[case] exp: f64,
    ) {
        let res = spot_price_from_reserves(r0, r1, t0d, t1d).expect("spot price");

        assert_ulps_eq!(res, exp);
    }

    #[rstest]
    #[case::empty_pool(U256::ZERO, U256::ZERO)]
    #[case::empty_reserve0(U256::ZERO, U256::from(5u8))]
    #[case::empty_reserve1(U256::from(5u8), U256::ZERO)]
    fn test_empty_reserve_has_no_liquidity(#[case] r0: U256, #[case] r1: U256) {
        let err = spot_price_from_reserves(r0, r1, 18, 18).unwrap_err();

        assert!(
            matches!(&err, SimulationError::RecoverableError(msg) if msg == "No liquidity"),
            "{err:?}"
        );
    }

    #[rstest]
    // 10^400 overflows an f64 to infinity.
    #[case::price_overflows(400, 0)]
    // 10^-400 underflows an f64 to zero.
    #[case::price_underflows(0, 400)]
    fn test_unrepresentable_price_is_invalid_input(#[case] t0d: u32, #[case] t1d: u32) {
        let err = spot_price_from_reserves(U256::ONE, U256::ONE, t0d, t1d).unwrap_err();

        assert!(matches!(err, SimulationError::InvalidInput(..)), "{err:?}");
    }
}
