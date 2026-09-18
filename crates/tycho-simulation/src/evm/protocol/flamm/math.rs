// Copyright (c) 2026 Everlong Labs Limited

//! Shared 256-bit integer helpers with the exact semantics of the Solidity they stand in for: every
//! division floors unless the name says otherwise, [`mul_div`] is OpenZeppelin 4.8 `Math.mulDiv`
//! over a 512-bit product and refuses exactly where it reverts, and [`sqrt`] is `Math.sqrt`'s floor
//! square root. Checked arithmetic that Solidity would panic on is surfaced as
//! [`FlammError::PanicArithmetic`] / [`FlammError::PanicDivZero`], never as a Rust panic.
//!
//! Two `mulDiv` families live here because the two contracts they serve differ on overflow:
//!
//! - [`mul_div`] / [`mul_div_up`] are OpenZeppelin 4.8 `Math.mulDiv` with its reverts
//!   (`lib/openzeppelin-contracts/contracts/utils/math/Math.sol:55-132`, `:140-151`), for every
//!   contract that runs under checked arithmetic and must surface each revert.
//! - [`mul_div_floor_raw`] / [`mul_div_up_raw`] are the same quotients without a revert path, for
//!   `CollRebalancerMath`, which bounds its inputs by `MAX_INPUT` before any product forms and
//!   never reverts from a public entrypoint. They return the low 256 bits of the quotient when it
//!   does not fit and zero on a zero denominator, exactly as the Go port's `mulDiv` / `mulDivCeil`
//!   (`levcurve.go`) do, so the private curve helpers agree with that port word for word even when
//!   a test drives them past their callers' domain.
//!
//! [`mul512`] and [`product_gt`] are `Mul512.mul512` / `Mul512.productGt`
//! (`src/libraries/math/Mul512.sol:14-27`). One `ceilDiv`: [`div_ceil`] is OpenZeppelin's, which
//! `FLAMMGateLib.netPW` / `navAt` (`FLAMMGateLib.sol:51`, `:146`), `FLAMMSwapLib.fromN18Ceil`
//! (`FLAMMSwapLib.sol:50`) and `FLAMMLeverLib.payNative` (`FLAMMLeverLib.sol:140`) all call; the Go
//! port's `divCeil` (`math_common.go:36`) differs from it only at a zero divisor, which the gate
//! never passes (`netPW` and `navAt` return or revert on `priceWad == 0` first). `U256`'s `+`, `-`
//! and `*` operators wrap, as `uint256` does under `unchecked`; the checked forms are
//! [`checked_add`], [`checked_sub`], [`checked_mul`], [`signed_add_checked`] and
//! [`signed_sub_checked`].
//!
//! `tycho-simulation`'s own `safe_math` helpers answer a `SimulationError` and do not tell a
//! `Panic(0x12)` from `Math.mulDiv`'s bare revert, so the port keeps its own family: the revert
//! class is part of what a quote must reproduce.

use alloy::primitives::{U256, U512};

use super::error::FlammError;

/// `1e18`.
pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// `5e17`, `WAD / 2`: the curve's anchor coordinate and the fee law's tie weight.
pub const HALF_WAD: U256 = U256::from_limbs([500_000_000_000_000_000, 0, 0, 0]);
/// `WAD * WAD = 1e36`.
pub const WAD_SQUARED: U256 =
    U256::from_limbs([0xb34b_9f10_0000_0000, 0x00c0_97ce_7bc9_0715, 0, 0]);
/// `2**96`.
pub const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);

/// OpenZeppelin 4.8 `Math.mulDiv(x, y, d)`
/// (`lib/openzeppelin-contracts/contracts/utils/math/Math.sol:55-132`): `floor(x * y / d)` over the
/// 512-bit product. A product that fits 256 bits divides with Solidity's `/`, so a zero denominator
/// is `Panic(0x12)`; a wider product hits the bare `require(denominator > prod1)` (empty revert
/// data) when the quotient does not fit 256 bits or the denominator is zero.
pub fn mul_div(x: U256, y: U256, d: U256) -> Result<U256, FlammError> {
    let prod: U512 = x.widening_mul(y);
    let (lo, hi) = split(prod);
    if hi.is_zero() {
        // `prod0 / denominator` (Math.sol:74): a plain EVM division of a 256-bit product.
        return if d.is_zero() { Err(FlammError::PanicDivZero) } else { Ok(lo / d) };
    }
    if d <= hi {
        // `require(denominator > prod1)` (Math.sol:78), which a zero denominator also fails.
        return Err(FlammError::MulDivOverflow);
    }
    let (q, q_hi) = split(prod / U512::from(d));
    debug_assert!(q_hi.is_zero(), "denominator > prod1 bounds the quotient below 2^256");
    Ok(q)
}

/// `Math.mulDiv(x, y, d, Rounding.Up)` (`Math.sol:140-151`): the floor plus one on a non-zero
/// `mulmod(x, y, d)`, the increment checked (`Panic(0x11)` at the 256-bit ceiling).
pub fn mul_div_up(x: U256, y: U256, d: U256) -> Result<U256, FlammError> {
    let z = mul_div(x, y, d)?;
    if x.mul_mod(y, d).is_zero() {
        return Ok(z);
    }
    checked_add(z, U256::from(1))
}

/// `Math.sqrt(a)` (`Math.sol:158-190`): the floor square root. Newton's iteration from an
/// over-estimate decreases monotonically onto `floor(sqrt(a))`, the value OZ's `min(result, a /
/// result)` settles on.
pub fn sqrt(a: U256) -> U256 {
    if a.is_zero() {
        return U256::ZERO;
    }
    // 2^ceil(bitlen / 2) >= sqrt(a).
    let mut x = U256::from(1) << a.bit_len().div_ceil(2);
    loop {
        let y = (x + a / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Solidity checked `a + b`.
pub fn checked_add(a: U256, b: U256) -> Result<U256, FlammError> {
    a.checked_add(b)
        .ok_or(FlammError::PanicArithmetic)
}

/// Solidity checked `a - b`.
pub fn checked_sub(a: U256, b: U256) -> Result<U256, FlammError> {
    a.checked_sub(b)
        .ok_or(FlammError::PanicArithmetic)
}

/// Solidity checked `a * b`.
pub fn checked_mul(a: U256, b: U256) -> Result<U256, FlammError> {
    a.checked_mul(b)
        .ok_or(FlammError::PanicArithmetic)
}

/// `a - b` when `a > b`, else zero: the `a > b ? a - b : 0` idiom the curve uses on every output
/// leg.
pub fn sat_sub(a: U256, b: U256) -> U256 {
    a.saturating_sub(b)
}

/// `Math.ceilDiv(a, b)` (`Math.sol:45-48`): `a == 0 ? 0 : (a - 1) / b + 1`, so a zero divisor is
/// `Panic(0x12)` only for a non-zero dividend.
pub fn div_ceil(a: U256, b: U256) -> Result<U256, FlammError> {
    if a.is_zero() {
        return Ok(U256::ZERO);
    }
    if b.is_zero() {
        return Err(FlammError::PanicDivZero);
    }
    Ok((a - U256::from(1)) / b + U256::from(1))
}

/// Solidity checked `a / b`: `Panic(0x12)` on a zero divisor.
pub fn checked_div(a: U256, b: U256) -> Result<U256, FlammError> {
    a.checked_div(b)
        .ok_or(FlammError::PanicDivZero)
}

/// Solidity `a / b`: the floor quotient, `Panic(0x12)` on a zero divisor ([`checked_div`] under
/// the name the curve and hook ports use).
pub fn div(a: U256, b: U256) -> Result<U256, FlammError> {
    checked_div(a, b)
}

/// The smaller of `a` and `b` (`b` on a tie), the `a < b ? a : b` idiom.
pub fn min_u(a: U256, b: U256) -> U256 {
    if a < b {
        a
    } else {
        b
    }
}

/// `1e6`, `FLAMMLeverLib.PPM` / `EverlongLeverageHook.PPM`.
pub const PPM: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
/// `2**48 - 1`: the mask of a `uint48` timestamp.
pub const UINT48_MAX: u64 = (1 << 48) - 1;

/// Bit 255 of a word: the sign of the `int256` it reinterprets as.
const INT256_SIGN_BIT: u64 = 1 << 63;

/// `Mul512.mul512(a, b)`: the high and low 256-bit limbs of the full product (`Mul512.sol:14-20`).
pub fn mul512(a: U256, b: U256) -> (U256, U256) {
    let (lo, hi) = split(a.widening_mul(b));
    (hi, lo)
}

/// `Mul512.productGt(a, b, c, d)`: `a * b > c * d` over the full 512-bit products
/// (`Mul512.sol:23-27`).
pub fn product_gt(a: U256, b: U256, c: U256, d: U256) -> bool {
    a.widening_mul::<256, 4, 512, 8>(b) > c.widening_mul::<256, 4, 512, 8>(d)
}

/// `floor(x * y / d)` as the Go port's `mulDiv` (`levcurve.go`) reports it: the low 256 bits of the
/// 512-bit quotient and whether the quotient overflowed 256 bits; `(0, false)` on a zero
/// denominator.
pub fn mul_div_floor_raw(x: U256, y: U256, d: U256) -> (U256, bool) {
    if x.is_zero() || y.is_zero() || d.is_zero() {
        return (U256::ZERO, false);
    }
    let q: U512 = x.widening_mul::<256, 4, 512, 8>(y) / U512::from(d);
    let (lo, hi) = split(q);
    (lo, !hi.is_zero())
}

/// `ceil(x * y / d)` as the Go port's `mulDivCeil` (`levcurve.go`) reports it: the floor plus one
/// on a non-zero `mulmod(x, y, d)`, left as the floor when the floor overflowed or is already
/// `2^256 - 1`.
pub fn mul_div_up_raw(x: U256, y: U256, d: U256) -> (U256, bool) {
    let (z, overflow) = mul_div_floor_raw(x, y, d);
    if overflow {
        return (z, true);
    }
    if x.mul_mod(y, d).is_zero() {
        return (z, false);
    }
    if z == U256::MAX {
        return (z, true);
    }
    (z + U256::from(1), false)
}

/// `int256` checked `x + y` on the two's-complement words: the sum wraps, and a result whose sign
/// differs from both operands' shared sign is the overflow Solidity panics on.
pub fn signed_add_checked(x: U256, y: U256) -> Result<U256, FlammError> {
    let z = x.wrapping_add(y);
    let (xs, ys, zs) = (x.as_limbs()[3], y.as_limbs()[3], z.as_limbs()[3]);
    if (xs ^ zs) & (ys ^ zs) & INT256_SIGN_BIT != 0 {
        return Err(FlammError::PanicArithmetic);
    }
    Ok(z)
}

/// `int256` checked `x - y` on the two's-complement words: overflow when the operands' signs differ
/// and the result's sign differs from `x`'s.
pub fn signed_sub_checked(x: U256, y: U256) -> Result<U256, FlammError> {
    let z = x.wrapping_sub(y);
    let (xs, ys, zs) = (x.as_limbs()[3], y.as_limbs()[3], z.as_limbs()[3]);
    if (xs ^ ys) & (xs ^ zs) & INT256_SIGN_BIT != 0 {
        return Err(FlammError::PanicArithmetic);
    }
    Ok(z)
}

/// Whether the word, read as `int256`, is strictly positive.
pub fn int256_is_positive(x: U256) -> bool {
    !x.is_zero() && x.as_limbs()[3] & INT256_SIGN_BIT == 0
}

/// The low and high 256-bit halves of a 512-bit word.
fn split(v: U512) -> (U256, U256) {
    let l = v.as_limbs();
    (U256::from_limbs([l[0], l[1], l[2], l[3]]), U256::from_limbs([l[4], l[5], l[6], l[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(WAD, U256::from(10u64).pow(U256::from(18)));
        assert_eq!(HALF_WAD, WAD / U256::from(2));
        assert_eq!(WAD_SQUARED, WAD * WAD);
        assert_eq!(Q96, U256::from(1) << 96);
    }

    #[test]
    fn mul_div_semantics() {
        assert_eq!(mul_div(U256::from(7), U256::from(3), U256::from(2)), Ok(U256::from(10)));
        assert_eq!(mul_div_up(U256::from(7), U256::from(3), U256::from(2)), Ok(U256::from(11)));
        assert_eq!(mul_div_up(U256::from(6), U256::from(3), U256::from(2)), Ok(U256::from(9)));
        assert_eq!(
            mul_div(U256::from(1), U256::from(1), U256::ZERO),
            Err(FlammError::PanicDivZero)
        );
        assert_eq!(mul_div(U256::MAX, U256::from(2), U256::ZERO), Err(FlammError::MulDivOverflow));
        assert_eq!(
            mul_div(U256::MAX, U256::from(2), U256::from(1)),
            Err(FlammError::MulDivOverflow)
        );
        assert_eq!(mul_div(U256::MAX, U256::from(2), U256::from(2)), Ok(U256::MAX));
        assert_eq!(
            mul_div(U256::MAX, U256::from(4), U256::from(3)),
            Err(FlammError::MulDivOverflow)
        );
        assert_eq!(mul_div(U256::MAX, U256::MAX, U256::MAX), Ok(U256::MAX));
        assert_eq!(mul_div_up(U256::MAX, U256::from(3), U256::from(3)), Ok(U256::MAX));
        // x * y = 2^257 - 1 = 2 * (2^256 - 1) + 1: the floor is type(uint256).max with a remainder,
        // so the rounded-up `+= 1` is Panic(0x11)
        let x = U256::from(535_006_138_814_359u64);
        let y: U256 = "432862656469423142931042426214547535783388063929571229938474969"
            .parse()
            .unwrap();
        assert_eq!(mul_div(x, y, U256::from(2)), Ok(U256::MAX));
        assert_eq!(mul_div_up(x, y, U256::from(2)), Err(FlammError::PanicArithmetic));
        // A 512-bit product with a quotient that fits.
        let big = U256::from(1) << 200;
        assert_eq!(mul_div(big, big, U256::from(1) << 250), Ok(U256::from(1) << 150));
        assert_eq!(mul_div(big, big + U256::from(1), big), Ok(big + U256::from(1)));
    }

    #[test]
    fn div_ceil_semantics() {
        assert_eq!(div_ceil(U256::ZERO, U256::ZERO), Ok(U256::ZERO));
        assert_eq!(div_ceil(U256::from(1), U256::ZERO), Err(FlammError::PanicDivZero));
        assert_eq!(div_ceil(U256::from(7), U256::from(2)), Ok(U256::from(4)));
        assert_eq!(div_ceil(U256::from(8), U256::from(2)), Ok(U256::from(4)));
        assert_eq!(div_ceil(U256::MAX, U256::from(1)), Ok(U256::MAX));
        assert_eq!(div_ceil(U256::MAX, U256::MAX), Ok(U256::from(1)));
        assert_eq!(checked_div(U256::from(7), U256::ZERO), Err(FlammError::PanicDivZero));
        assert_eq!(div(U256::from(7), U256::ZERO), Err(FlammError::PanicDivZero));
        assert_eq!(div(U256::from(7), U256::from(2)), Ok(U256::from(3)));
        assert_eq!(min_u(U256::from(2), U256::from(3)), U256::from(2));
        assert_eq!(min_u(U256::from(3), U256::from(2)), U256::from(2));
        assert_eq!(min_u(U256::from(3), U256::from(3)), U256::from(3));
        assert_eq!(PPM, U256::from(1_000_000u64));
        assert_eq!(UINT48_MAX, 0xffff_ffff_ffff);
    }

    #[test]
    fn sqrt_is_floor() {
        for a in [0u64, 1, 2, 3, 4, 8, 9, 15, 16, 17, 99, 100, 101, u64::MAX] {
            let r = sqrt(U256::from(a));
            assert!(r * r <= U256::from(a));
            assert!((r + U256::from(1)) * (r + U256::from(1)) > U256::from(a));
        }
        let r = sqrt(U256::MAX);
        assert_eq!(r, (U256::from(1) << 128) - U256::from(1));
        for big in [
            (U256::from(1) << 200) - U256::from(1),
            U256::from(1) << 255,
            (U256::from(1) << 128) - U256::from(1),
        ] {
            let r = sqrt(big);
            let sq: U512 = r.widening_mul(r);
            assert!(sq <= U512::from(big));
            let r1 = r + U256::from(1);
            let sq1: U512 = r1.widening_mul(r1);
            assert!(sq1 > U512::from(big));
        }
    }

    #[test]
    fn mul512_and_product_gt() {
        let u = |v: u64| U256::from(v);
        let (hi, lo) = mul512(U256::MAX, U256::MAX);
        assert_eq!(hi, U256::MAX - u(1));
        assert_eq!(lo, u(1));
        let (hi, lo) = mul512(u(7), u(3));
        assert_eq!((hi, lo), (U256::ZERO, u(21)));
        assert!(product_gt(U256::MAX, u(2), U256::MAX, u(1)));
        assert!(!product_gt(U256::MAX, u(2), U256::MAX, u(2)));
        assert!(!product_gt(u(3), u(4), u(6), u(2)));
        assert!(product_gt(u(3), u(5), u(6), u(2)));
    }

    #[test]
    fn raw_mul_div_semantics() {
        let u = |v: u64| U256::from(v);
        assert_eq!(mul_div_floor_raw(u(7), u(3), u(2)), (u(10), false));
        assert_eq!(mul_div_up_raw(u(7), u(3), u(2)), (u(11), false));
        assert_eq!(mul_div_floor_raw(u(7), u(3), U256::ZERO), (U256::ZERO, false));
        assert_eq!(mul_div_up_raw(u(7), u(3), U256::ZERO), (U256::ZERO, false));
        // The quotient's low limbs on overflow, exactly as holiman/uint256 MulDivOverflow reports
        // them.
        assert_eq!(mul_div_floor_raw(U256::MAX, u(4), u(2)), (U256::MAX - u(1), true));
        assert_eq!(mul_div_up_raw(U256::MAX, u(4), u(2)), (U256::MAX - u(1), true));
        let half = U256::from(1) << 255;
        assert_eq!(mul_div_up_raw(U256::MAX - u(1), half + u(1), half), (U256::MAX, true));
        // (2^256 - 2) * (2^255 + 1) = 2^511 - 2: quotient 2^256 - 1 with remainder 2^255 - 2 over
        // 2^255.
        assert_eq!(mul_div(U256::MAX - u(1), half + u(1), half), Ok(U256::MAX));
        assert_eq!(
            mul_div_up(U256::MAX - u(1), half + u(1), half),
            Err(FlammError::PanicArithmetic)
        );
    }

    #[test]
    fn signed_checked_ops() {
        let u = |v: u64| U256::from(v);
        let neg_one = U256::MAX;
        let max_pos = U256::MAX >> 1;
        let min_neg = U256::from(1) << 255;
        assert_eq!(signed_add_checked(u(5), neg_one), Ok(u(4)));
        assert_eq!(signed_add_checked(max_pos, u(1)), Err(FlammError::PanicArithmetic));
        assert_eq!(signed_add_checked(min_neg, neg_one), Err(FlammError::PanicArithmetic));
        assert_eq!(signed_add_checked(min_neg, max_pos), Ok(neg_one));
        assert_eq!(signed_sub_checked(u(3), u(5)), Ok(U256::MAX - u(1)));
        assert_eq!(signed_sub_checked(min_neg, u(1)), Err(FlammError::PanicArithmetic));
        assert_eq!(signed_sub_checked(max_pos, neg_one), Err(FlammError::PanicArithmetic));
        assert_eq!(signed_sub_checked(neg_one, max_pos), Ok(min_neg));
        assert!(int256_is_positive(u(1)));
        assert!(!int256_is_positive(U256::ZERO));
        assert!(!int256_is_positive(neg_one));
        assert!(int256_is_positive(max_pos));
    }
}
