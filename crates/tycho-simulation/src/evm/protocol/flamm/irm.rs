// Copyright (c) 2026 Everlong Labs Limited

//! The market's rate model: Morpho's `AdaptiveCurveIrm`
//! (`0x46415998764C29aB2a25CbeA6254146D50D22687` on Base, the IRM of the c104 venue market
//! `0x9103c3b4…1836` per Blue's `idToMarketParams`), ported from morpho-org/morpho-blue-irm v1.0.0
//! (`src/AdaptiveCurveIrm.sol`, `src/libraries/adaptive-curve/ExpLib.sol`, `src/libraries/
//! adaptive-curve/ConstantsLib.sol`, `src/libraries/MathLib.sol`, `src/libraries/UtilsLib.sol`).
//!
//! The arithmetic is `int256`: `wMulToZero` / `wDivToZero` truncate toward zero (`I256`'s
//! division), `wExp` decomposes over `ln 2` and shifts a positive `e^r`. Every checked `int256`
//! operation of `_borrowRate` is checked here too and surfaces as [`FlammError::PanicArithmetic`],
//! as is Blue's own `block.timestamp - lastUpdate` underflow; given `uint128` market totals none of
//! the multiplications can overflow (the largest, `coeff * err`, stays below `2^253`), so the
//! timestamp underflow is the only revert reachable from tracked state.

use alloy::primitives::{I256, U256};

use super::{error::FlammError, morpho::Market};

/// `WAD_INT = int256(1e18)` (`MathLib.sol:6`).
const WAD_INT: I256 = I256::from_raw(U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]));
/// `ConstantsLib.CURVE_STEEPNESS = 4 ether` (`ConstantsLib.sol:10`).
const CURVE_STEEPNESS: I256 =
    I256::from_raw(U256::from_limbs([4_000_000_000_000_000_000, 0, 0, 0]));
/// `ConstantsLib.ADJUSTMENT_SPEED = 50 ether / int256(365 days)` (`ConstantsLib.sol:16`), the
/// quotient truncated as Solidity's constant folding does (`50e18 / 31536000`).
const ADJUSTMENT_SPEED: I256 = I256::from_raw(U256::from_limbs([1_585_489_599_188, 0, 0, 0]));
/// `ConstantsLib.TARGET_UTILIZATION = 0.9 ether` (`ConstantsLib.sol:20`).
const TARGET_UTILIZATION: I256 =
    I256::from_raw(U256::from_limbs([900_000_000_000_000_000, 0, 0, 0]));
/// `ConstantsLib.INITIAL_RATE_AT_TARGET = 0.04 ether / int256(365 days)` (`ConstantsLib.sol:24`).
const INITIAL_RATE_AT_TARGET: I256 =
    I256::from_raw(U256::from_limbs([40_000_000_000_000_000 / (365 * 86_400), 0, 0, 0]));
/// `ConstantsLib.MIN_RATE_AT_TARGET = 0.001 ether / int256(365 days)` (`ConstantsLib.sol:28`).
const MIN_RATE_AT_TARGET: I256 =
    I256::from_raw(U256::from_limbs([1_000_000_000_000_000 / (365 * 86_400), 0, 0, 0]));
/// `ConstantsLib.MAX_RATE_AT_TARGET = 2.0 ether / int256(365 days)` (`ConstantsLib.sol:32`).
const MAX_RATE_AT_TARGET: I256 =
    I256::from_raw(U256::from_limbs([2_000_000_000_000_000_000 / (365 * 86_400), 0, 0, 0]));
/// `ExpLib.LN_2_INT = 0.693147180559945309 ether` (`ExpLib.sol:12`).
const LN_2_INT: I256 = I256::from_raw(U256::from_limbs([693_147_180_559_945_309, 0, 0, 0]));
/// `ExpLib.LN_WEI_INT = -41.446531673892822312 ether` (`ExpLib.sol:15`), as two's complement.
const LN_WEI_INT: I256 = I256::from_raw(U256::from_limbs([
    13_893_700_547_235_832_536,
    u64::MAX - 2,
    u64::MAX,
    u64::MAX,
]));
/// `ExpLib.WEXP_UPPER_BOUND = 93.859467695000404319 ether` (`ExpLib.sol:19`).
const WEXP_UPPER_BOUND: I256 =
    I256::from_raw(U256::from_limbs([1_625_747_326_452_646_239, 5, 0, 0]));
/// `ExpLib.WEXP_UPPER_VALUE = 57716089161558943949701069502944508345128.422502756744429568 ether`
/// (`ExpLib.sol:22`).
const WEXP_UPPER_VALUE: I256 = I256::from_raw(U256::from_limbs([0, 0, 0x31d8_1650_c7d8_8b80, 0x9]));

/// `AdaptiveCurveIrm._borrowRate(id, market)` (`AdaptiveCurveIrm.sol:76-130`) at `now`, as
/// `(avgRate, endRateAtTarget)`: the view (`borrowRateView`, `:53-56`) returns `avgRate`; Blue's
/// accrual calls `borrowRate` (`:59-72`), which also stores `endRateAtTarget`. `rate_at_target` is
/// the stored `int256` slot word.
pub fn borrow_rate(m: &Market, rate_at_target: U256, now: u64) -> Result<(U256, U256), FlammError> {
    // utilization = tsa > 0 ? wDivDown(tba, tsa) : 0 -- MorphoMathLib.wDivDown on the uint128
    // totals, then the "safe unchecked" int256 cast (:78-79).
    let utilization = if m.total_supply_assets.is_zero() {
        I256::ZERO
    } else {
        let u = super::morpho::w_div_down(m.total_borrow_assets, m.total_supply_assets)?;
        I256::from_raw(u)
    };
    let err_norm_factor = if utilization > TARGET_UTILIZATION {
        WAD_INT - TARGET_UTILIZATION
    } else {
        TARGET_UTILIZATION
    };
    let err = w_div_to_zero(checked_sub_i(utilization, TARGET_UTILIZATION)?, err_norm_factor)?;

    let start = I256::from_raw(rate_at_target);
    let (avg_rate_at_target, end_rate_at_target) = if start.is_zero() {
        // first interaction (:91-94)
        (INITIAL_RATE_AT_TARGET, INITIAL_RATE_AT_TARGET)
    } else {
        let speed = w_mul_to_zero(ADJUSTMENT_SPEED, err)?;
        // `int256(block.timestamp - market.lastUpdate)`: a checked uint256 subtraction (:101)
        let now_u = U256::from(now);
        if now_u < m.last_update {
            return Err(FlammError::PanicArithmetic);
        }
        let elapsed = I256::from_raw(now_u - m.last_update);
        let linear_adaptation = checked_mul_i(speed, elapsed)?;
        if linear_adaptation.is_zero() {
            (start, start)
        } else {
            // trapezoid with N = 2: (start + end + 2 * mid) / 4, each leg bounded to [MIN, MAX]
            // (:122-124)
            let end = new_rate_at_target(start, linear_adaptation)?;
            let mid = new_rate_at_target(start, linear_adaptation / I256::from_raw(U256::from(2)))?;
            let sum = checked_add_i(
                checked_add_i(start, end)?,
                checked_mul_i(I256::from_raw(U256::from(2)), mid)?,
            )?;
            (sum / I256::from_raw(U256::from(4)), end)
        }
    };
    // `uint256(_curve(avgRateAtTarget, err))`, the cast wrapping (:129)
    Ok((curve(avg_rate_at_target, err)?.into_raw(), end_rate_at_target.into_raw()))
}

/// `AdaptiveCurveIrm._curve` (`AdaptiveCurveIrm.sol:136-141`): `((1 - 1/C) * err + 1) * rate` below
/// target, `((C - 1) * err + 1) * rate` above, both products `wMulToZero`.
fn curve(rate_at_target: I256, err: I256) -> Result<I256, FlammError> {
    let coeff = if err.is_negative() {
        WAD_INT - w_div_to_zero(WAD_INT, CURVE_STEEPNESS)?
    } else {
        CURVE_STEEPNESS - WAD_INT
    };
    w_mul_to_zero(checked_add_i(w_mul_to_zero(coeff, err)?, WAD_INT)?, rate_at_target)
}

/// `AdaptiveCurveIrm._newRateAtTarget` (`AdaptiveCurveIrm.sol:145-150`):
/// `wMulToZero(start, wExp(linearAdaptation))` bounded to `[MIN_RATE_AT_TARGET,
/// MAX_RATE_AT_TARGET]` (`UtilsLib.bound`, `UtilsLib.sol:11-18`: `max(min(x, high), low)`).
fn new_rate_at_target(start: I256, linear_adaptation: I256) -> Result<I256, FlammError> {
    let z = w_mul_to_zero(start, w_exp(linear_adaptation))?;
    let z = if z > MAX_RATE_AT_TARGET { MAX_RATE_AT_TARGET } else { z };
    Ok(if z < MIN_RATE_AT_TARGET { MIN_RATE_AT_TARGET } else { z })
}

/// `ExpLib.wExp` (`ExpLib.sol:25-48`): zero below `ln(1e-18)`, clipped at `WEXP_UPPER_BOUND`, else
/// `x = q * ln2 + r` with `q` rounded half toward zero, `e^r` by a 2nd-order Taylor polynomial
/// `WAD + r + r*r/WAD/2`, and `e^x = e^r << q` (`>> -q`, an arithmetic shift of a positive value).
/// The block is `unchecked` on chain and every step is bounded, so nothing here can revert.
pub fn w_exp(x: I256) -> I256 {
    if x < LN_WEI_INT {
        return I256::ZERO;
    }
    if x >= WEXP_UPPER_BOUND {
        return WEXP_UPPER_VALUE;
    }
    let half_ln2 = LN_2_INT / I256::from_raw(U256::from(2));
    let adj = if x.is_negative() { -half_ln2 } else { half_ln2 };
    let q = (x + adj) / LN_2_INT;
    let r = x - q * LN_2_INT;
    let exp_r = WAD_INT + r + (r * r) / WAD_INT / I256::from_raw(U256::from(2));
    if q.is_negative() {
        exp_r.asr((-q).into_raw().to::<usize>())
    } else {
        exp_r << q.into_raw().to::<usize>()
    }
}

/// `MathLib.wMulToZero(x, y) = (x * y) / WAD_INT` (`MathLib.sol:14-16`), the product checked.
fn w_mul_to_zero(x: I256, y: I256) -> Result<I256, FlammError> {
    Ok(checked_mul_i(x, y)? / WAD_INT)
}

/// `MathLib.wDivToZero(x, y) = (x * WAD_INT) / y` (`MathLib.sol:19-21`), the product checked; the
/// divisor is a non-zero constant on every call site.
fn w_div_to_zero(x: I256, y: I256) -> Result<I256, FlammError> {
    checked_mul_i(x, WAD_INT)?
        .checked_div(y)
        .ok_or(FlammError::PanicArithmetic)
}

fn checked_add_i(x: I256, y: I256) -> Result<I256, FlammError> {
    x.checked_add(y)
        .ok_or(FlammError::PanicArithmetic)
}

fn checked_sub_i(x: I256, y: I256) -> Result<I256, FlammError> {
    x.checked_sub(y)
        .ok_or(FlammError::PanicArithmetic)
}

fn checked_mul_i(x: I256, y: I256) -> Result<I256, FlammError> {
    x.checked_mul(y)
        .ok_or(FlammError::PanicArithmetic)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(v: i128) -> I256 {
        I256::try_from(v).unwrap()
    }

    #[test]
    fn constants() {
        assert_eq!(ADJUSTMENT_SPEED, i(50_000_000_000_000_000_000 / (365 * 86_400)));
        assert_eq!(INITIAL_RATE_AT_TARGET, i(1_268_391_679));
        assert_eq!(MIN_RATE_AT_TARGET, i(31_709_791));
        assert_eq!(MAX_RATE_AT_TARGET, i(63_419_583_967));
        assert_eq!(LN_WEI_INT, i(-41_446_531_673_892_822_312));
        assert_eq!(WEXP_UPPER_BOUND, i(93_859_467_695_000_404_319));
        assert_eq!(
            WEXP_UPPER_VALUE,
            "57716089161558943949701069502944508345128422502756744429568"
                .parse::<I256>()
                .unwrap()
        );
        assert_eq!(WEXP_UPPER_VALUE, w_exp(WEXP_UPPER_BOUND));
        // the clip value is the unclipped formula at the bound
        let half_ln2 = LN_2_INT / i(2);
        let q = (WEXP_UPPER_BOUND + half_ln2) / LN_2_INT;
        let r = WEXP_UPPER_BOUND - q * LN_2_INT;
        let exp_r = WAD_INT + r + (r * r) / WAD_INT / i(2);
        assert_eq!(exp_r << q.into_raw().to::<usize>(), WEXP_UPPER_VALUE);
    }

    #[test]
    fn signed_division_truncates_toward_zero() {
        assert_eq!(i(-7) / i(2), i(-3));
        assert_eq!(i(7) / i(-2), i(-3));
        assert_eq!(i(-7).asr(1), i(-4));
    }

    #[test]
    fn w_exp_reference_points() {
        assert_eq!(w_exp(I256::ZERO), WAD_INT);
        assert_eq!(w_exp(LN_WEI_INT - i(1)), I256::ZERO);
        // e^ln2 = 2 exactly: q = 1, r = 0
        assert_eq!(w_exp(LN_2_INT), i(2_000_000_000_000_000_000));
        assert_eq!(w_exp(-LN_2_INT), i(500_000_000_000_000_000));
        // wExp(WEXP_UPPER_BOUND - 1) is just under the clip value and positive
        let below = w_exp(WEXP_UPPER_BOUND - i(1));
        assert!(below > I256::ZERO && below <= WEXP_UPPER_VALUE);
    }

    #[test]
    fn first_interaction_and_timestamp_underflow() {
        let m = Market { last_update: U256::from(100), ..Default::default() };
        assert_eq!(
            borrow_rate(&m, U256::ZERO, 50),
            Ok((U256::from(317_097_919u64), U256::from(1_268_391_679u64)))
        );
        assert_eq!(borrow_rate(&m, U256::from(1), 50), Err(FlammError::PanicArithmetic));
        // no adaptation without elapsed time: the curve at the stored rate, utilization zero
        assert_eq!(
            borrow_rate(&m, U256::from(1_268_391_679u64), 100),
            Ok((U256::from(317_097_919u64), U256::from(1_268_391_679u64)))
        );
    }
}
