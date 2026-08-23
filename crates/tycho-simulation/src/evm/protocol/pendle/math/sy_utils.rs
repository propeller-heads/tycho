//! Port of `SYUtils.sol` and `PYIndexLib` — converting between SY units and accounting-asset
//! units through the PY index.
//!
//! Provenance and licensing: see `../NOTICE.md`.
//!
//! This is the single easiest place in the whole integration to ship a silently wrong quote. Two
//! traps, both load-bearing:
//!
//! - **The index is a raw uint, not a rate in 18 decimals.** The `/ 1e18` in these formulas is the
//!   fixed-point scale of the *multiplication*, not a statement about the index's own decimals. The
//!   index absorbs the decimal gap between SY and the accounting asset: on the reUSD market,
//!   `exchangeRate()` reads `1_095_830` because SY has 18 decimals and the asset has 6. Treating
//!   the index as "1.09583e18" there is wrong by twelve orders of magnitude.
//! - **The rounding direction is chosen per call site.** The AMM converts the trader's side down
//!   and the protocol's side up, so the remainder always lands with the protocol. Swapping a `Down`
//!   for an `Up` is a one-wei error that the contract will refuse to settle.
//!
//! The curve runs in asset units: `totalPt` is already in them, `totalSy` is not, and these are
//! what bridge the two.

use alloy::primitives::{I256, U256};

use super::{
    errors::{PendleError, PendleResult},
    pmath,
};

fn mul(a: U256, b: U256, op: &'static str) -> PendleResult<U256> {
    a.checked_mul(b)
        .ok_or(PendleError::Overflow { operation: op })
}

fn require_non_zero(index: U256, op: &'static str) -> PendleResult<()> {
    if index.is_zero() {
        return Err(PendleError::DivisionByZero { operation: op });
    }
    Ok(())
}

/// SY → accounting asset, truncated.
pub fn sy_to_asset(index: U256, sy_amount: U256) -> PendleResult<U256> {
    Ok(mul(sy_amount, index, "syToAsset")? / pmath::one())
}

/// SY → accounting asset, rounded up.
pub fn sy_to_asset_up(index: U256, sy_amount: U256) -> PendleResult<U256> {
    let numerator = mul(sy_amount, index, "syToAssetUp")?
        .checked_add(pmath::one() - U256::from(1))
        .ok_or(PendleError::Overflow { operation: "syToAssetUp" })?;
    Ok(numerator / pmath::one())
}

/// Accounting asset → SY, truncated.
pub fn asset_to_sy(index: U256, asset_amount: U256) -> PendleResult<U256> {
    require_non_zero(index, "assetToSy")?;
    Ok(mul(asset_amount, pmath::one(), "assetToSy")? / index)
}

/// Accounting asset → SY, rounded up.
pub fn asset_to_sy_up(index: U256, asset_amount: U256) -> PendleResult<U256> {
    require_non_zero(index, "assetToSyUp")?;
    let numerator = mul(asset_amount, pmath::one(), "assetToSyUp")?
        .checked_add(index - U256::from(1))
        .ok_or(PendleError::Overflow { operation: "assetToSyUp" })?;
    Ok(numerator / index)
}

/// Signed SY → asset, converting the magnitude and reapplying the sign.
///
/// Note this is *not* the same as converting the signed value directly: the contract takes
/// `abs()`, converts, then negates, so a negative amount truncates toward zero rather than toward
/// negative infinity.
pub fn sy_to_asset_i(index: U256, sy_amount: I256) -> PendleResult<I256> {
    signed(index, sy_amount, sy_to_asset)
}

/// Signed asset → SY, converting the magnitude and reapplying the sign.
pub fn asset_to_sy_i(index: U256, asset_amount: I256) -> PendleResult<I256> {
    signed(index, asset_amount, asset_to_sy)
}

/// Signed asset → SY, rounded up on the magnitude.
pub fn asset_to_sy_up_i(index: U256, asset_amount: I256) -> PendleResult<I256> {
    signed(index, asset_amount, asset_to_sy_up)
}

fn signed(
    index: U256,
    amount: I256,
    convert: fn(U256, U256) -> PendleResult<U256>,
) -> PendleResult<I256> {
    let negative = amount.is_negative();
    let magnitude = amount.unsigned_abs();
    let converted = pmath::to_i256(convert(index, magnitude)?)?;
    Ok(if negative { -converted } else { converted })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wstETH market: SY and accounting asset both 18 decimals, index just above one.
    /// `pyIndexStored` as the brief quotes it.
    const WSTETH_INDEX: u128 = 1_241_884_000_000_000_000;

    /// The reUSD market: SY has 18 decimals, PT and the accounting asset have 6, and the index
    /// absorbs the gap. Read straight off chain — this is why the index cannot be assumed 1e18.
    const REUSD_INDEX: u128 = 1_095_830;

    fn u(value: u128) -> U256 {
        U256::from(value)
    }

    fn s(value: i128) -> I256 {
        I256::try_from(value).unwrap()
    }

    #[test]
    fn sy_to_asset_scales_by_the_index() {
        // 1 SY at an index of 1.241884 is 1.241884 units of the asset.
        assert_eq!(
            sy_to_asset(u(WSTETH_INDEX), u(1_000_000_000_000_000_000)).unwrap(),
            u(1_241_884_000_000_000_000)
        );
    }

    /// The decimal axis. One whole SY (18 decimals) at the reUSD index yields ~1.09583 units of a
    /// 6-decimal asset — not 1.09583e18. A port that hard-codes a 1e18 index returns a number
    /// twelve orders of magnitude wrong, and every downstream quote inherits it.
    #[test]
    fn the_index_carries_the_decimal_gap() {
        let one_sy = u(1_000_000_000_000_000_000);
        assert_eq!(sy_to_asset(u(REUSD_INDEX), one_sy).unwrap(), u(1_095_830));
        // And back again, to the same order of magnitude.
        assert_eq!(asset_to_sy(u(REUSD_INDEX), u(1_095_830)).unwrap(), one_sy);
    }

    /// Up and down differ by exactly one wei when the division leaves a remainder, and not at all
    /// when it divides evenly. Getting this backwards is a one-wei error the contract rejects.
    #[test]
    fn rounding_direction_differs_only_on_a_remainder() {
        // 3 * (1e18/3) leaves a remainder against ONE.
        let amount = u(1);
        let index = u(3);
        assert_eq!(sy_to_asset(index, amount).unwrap(), U256::ZERO);
        assert_eq!(sy_to_asset_up(index, amount).unwrap(), u(1));

        let exact = u(1_000_000_000_000_000_000);
        assert_eq!(sy_to_asset(exact, exact).unwrap(), exact);
        assert_eq!(sy_to_asset_up(exact, exact).unwrap(), exact);
    }

    #[test]
    fn asset_to_sy_rounds_up_only_on_a_remainder() {
        assert_eq!(asset_to_sy(u(3), u(1)).unwrap(), u(333_333_333_333_333_333));
        assert_eq!(asset_to_sy_up(u(3), u(1)).unwrap(), u(333_333_333_333_333_334));
    }

    /// The contract converts `abs()` and reapplies the sign, so a negative magnitude truncates
    /// toward zero. Converting the signed value directly would floor toward negative infinity and
    /// differ by one wei.
    #[test]
    fn signed_conversion_truncates_toward_zero() {
        assert_eq!(sy_to_asset_i(u(3), s(-1)).unwrap(), I256::ZERO);
        assert_eq!(sy_to_asset_i(u(3), s(1)).unwrap(), I256::ZERO);
        assert_eq!(
            sy_to_asset_i(u(WSTETH_INDEX), s(-1_000_000_000_000_000_000)).unwrap(),
            s(-1_241_884_000_000_000_000)
        );
    }

    #[test]
    fn a_zero_index_is_an_error_not_a_panic() {
        assert!(matches!(asset_to_sy(U256::ZERO, u(1)), Err(PendleError::DivisionByZero { .. })));
        assert!(matches!(
            asset_to_sy_up(U256::ZERO, u(1)),
            Err(PendleError::DivisionByZero { .. })
        ));
        // The other direction multiplies, so a zero index is merely zero.
        assert_eq!(sy_to_asset(U256::ZERO, u(1)).unwrap(), U256::ZERO);
    }
}
