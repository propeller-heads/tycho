//! Port of `PMath.sol` — 18-decimal fixed-point arithmetic with the contract's rounding.
//!
//! Provenance and licensing: see `../NOTICE.md`.
//!
//! Only the parts the quote path reaches are ported. The rounding *direction* is the whole point
//! of this module: Pendle picks `Down` or `Up` per call site so that every rounding error falls in
//! the protocol's favour, and a port that rounds the other way is off by one in the user's favour
//! — which the contract will then refuse to settle.

use alloy::primitives::{I256, U256};

use super::errors::{PendleError, PendleResult};

/// One, in 18 decimals.
pub fn one() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

/// One, in 18 decimals, signed.
pub fn i_one() -> I256 {
    I256::try_from(1_000_000_000_000_000_000i128).expect("fits")
}

fn mul_u(a: U256, b: U256, op: &'static str) -> PendleResult<U256> {
    a.checked_mul(b)
        .ok_or(PendleError::Overflow { operation: op })
}

fn div_u(a: U256, b: U256, op: &'static str) -> PendleResult<U256> {
    if b.is_zero() {
        return Err(PendleError::DivisionByZero { operation: op });
    }
    Ok(a / b)
}

/// `a * b / 1e18`, truncated.
pub fn mul_down(a: U256, b: U256) -> PendleResult<U256> {
    div_u(mul_u(a, b, "mulDown")?, one(), "mulDown")
}

/// Signed `a * b / 1e18`.
///
/// Solidity's integer division truncates toward zero, and so does Rust's, so a negative product
/// rounds the same way in both.
pub fn mul_down_i(a: I256, b: I256) -> PendleResult<I256> {
    let product = a
        .checked_mul(b)
        .ok_or(PendleError::Overflow { operation: "mulDown" })?;
    product
        .checked_div(i_one())
        .ok_or(PendleError::Overflow { operation: "mulDown" })
}

/// `a * 1e18 / b`, truncated.
pub fn div_down(a: U256, b: U256) -> PendleResult<U256> {
    div_u(mul_u(a, one(), "divDown")?, b, "divDown")
}

/// Signed `a * 1e18 / b`.
pub fn div_down_i(a: I256, b: I256) -> PendleResult<I256> {
    if b.is_zero() {
        return Err(PendleError::DivisionByZero { operation: "divDown" });
    }
    let inflated = a
        .checked_mul(i_one())
        .ok_or(PendleError::Overflow { operation: "divDown" })?;
    inflated
        .checked_div(b)
        .ok_or(PendleError::Overflow { operation: "divDown" })
}

/// `ceil(a / b)` on plain integers, no fixed-point scaling.
pub fn raw_div_up(a: U256, b: U256) -> PendleResult<U256> {
    if b.is_zero() {
        return Err(PendleError::DivisionByZero { operation: "rawDivUp" });
    }
    let numerator = a
        .checked_add(b)
        .and_then(|v| v.checked_sub(U256::from(1)))
        .ok_or(PendleError::Overflow { operation: "rawDivUp" })?;
    Ok(numerator / b)
}

/// `max(a - b, 0)`.
pub fn sub_max_0(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        U256::ZERO
    }
}

/// `a - b`, refusing to go negative.
///
/// The contract's `require(a >= b, "negative")`. Reaching it means the caller asked for more than
/// the market holds, which is a quote that must fail rather than wrap.
pub fn sub_no_neg(a: I256, b: I256) -> PendleResult<I256> {
    if a < b {
        return Err(PendleError::NegativeResult { a: a.to_string(), b: b.to_string() });
    }
    a.checked_sub(b)
        .ok_or(PendleError::Overflow { operation: "subNoNeg" })
}

/// `PMath.Int`: unsigned to signed, guarded.
pub fn to_i256(x: U256) -> PendleResult<I256> {
    I256::try_from(x)
        .map_err(|_| PendleError::CastOutOfRange { value: x.to_string(), target: "int256" })
}

/// `PMath.Uint`: signed to unsigned, guarded on negatives.
pub fn to_u256(x: I256) -> PendleResult<U256> {
    if x.is_negative() {
        return Err(PendleError::CastOutOfRange { value: x.to_string(), target: "uint256" });
    }
    Ok(x.into_raw())
}

/// `a * (1 + factor)`.
pub fn tweak_up(a: U256, factor: U256) -> PendleResult<U256> {
    mul_down(a, one() + factor)
}

/// `a * (1 - factor)`.
pub fn tweak_down(a: U256, factor: U256) -> PendleResult<U256> {
    mul_down(a, one() - factor)
}

pub fn clamp(x: U256, lower: U256, upper: U256) -> U256 {
    if x < lower {
        lower
    } else if x > upper {
        upper
    } else {
        x
    }
}

/// `min(a + b, bound)`, saturating rather than overflowing.
pub fn add_with_upper_bound(a: U256, b: U256, bound: U256) -> U256 {
    match a.checked_add(b) {
        Some(sum) => sum.min(bound),
        None => bound,
    }
}

/// `max(a - b, bound)`, flooring rather than underflowing.
pub fn sub_with_lower_bound(a: U256, b: U256, bound: U256) -> U256 {
    if b > a {
        bound
    } else {
        (a - b).max(bound)
    }
}

/// `a <= b && a >= b * (1 - eps)`: the acceptance test the router's approximation loop uses.
pub fn is_a_smaller_approx_b(a: U256, b: U256, eps: U256) -> PendleResult<bool> {
    Ok(a <= b && a >= mul_down(b, one() - eps)?)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    fn u(value: u128) -> U256 {
        U256::from(value)
    }

    fn s(value: i128) -> I256 {
        I256::try_from(value).unwrap()
    }

    #[derive(Deserialize)]
    struct Case {
        op: String,
        a: String,
        b: String,
        c: String,
        y: String,
    }

    #[derive(Deserialize)]
    struct Fixtures {
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    struct RevertCase {
        case: String,
    }

    #[derive(Deserialize)]
    struct RevertFixtures {
        reverts: Vec<RevertCase>,
    }

    fn pu(value: &str) -> U256 {
        U256::from_str_radix(value, 10).expect("fixture holds a decimal integer")
    }

    fn pi(value: &str) -> I256 {
        I256::from_dec_str(value).expect("fixture holds a decimal integer")
    }

    /// Bit-equality against `PMath` itself, helper by helper.
    ///
    /// The market and approximation fixtures already exercise most of this library, but only along
    /// the paths those take. This pins each helper on operands chosen for it — chiefly ones that do
    /// not divide evenly, since a grid of exact divisions agrees with either rounding direction.
    #[test]
    fn pmath_matches_the_contract() {
        let fixtures: Fixtures =
            serde_json::from_str(include_str!("../tests/fixtures/pmath.json")).unwrap();
        assert!(fixtures.cases.len() >= 80, "fixture grid shrank unexpectedly");

        for case in &fixtures.cases {
            let label = format!("{}({}, {}, {})", case.op, case.a, case.b, case.c);
            let (a, b, c) = (case.a.as_str(), case.b.as_str(), case.c.as_str());

            match case.op.as_str() {
                "mul_down_u" => assert_eq!(mul_down(pu(a), pu(b)).unwrap(), pu(&case.y), "{label}"),
                "div_down_u" => assert_eq!(div_down(pu(a), pu(b)).unwrap(), pu(&case.y), "{label}"),
                "raw_div_up" => {
                    assert_eq!(raw_div_up(pu(a), pu(b)).unwrap(), pu(&case.y), "{label}")
                }
                "sub_max_0" => assert_eq!(sub_max_0(pu(a), pu(b)), pu(&case.y), "{label}"),
                "tweak_up" => assert_eq!(tweak_up(pu(a), pu(b)).unwrap(), pu(&case.y), "{label}"),
                "tweak_down" => {
                    assert_eq!(tweak_down(pu(a), pu(b)).unwrap(), pu(&case.y), "{label}")
                }
                "clamp" => assert_eq!(clamp(pu(a), pu(b), pu(c)), pu(&case.y), "{label}"),
                "add_with_upper_bound" => {
                    assert_eq!(add_with_upper_bound(pu(a), pu(b), pu(c)), pu(&case.y), "{label}")
                }
                "sub_with_lower_bound" => {
                    assert_eq!(sub_with_lower_bound(pu(a), pu(b), pu(c)), pu(&case.y), "{label}")
                }
                "is_a_smaller_approx_b" => assert_eq!(
                    is_a_smaller_approx_b(pu(a), pu(b), pu(c)).unwrap(),
                    case.y == "1",
                    "{label}"
                ),
                "mul_down_i" => {
                    assert_eq!(mul_down_i(pi(a), pi(b)).unwrap(), pi(&case.y), "{label}")
                }
                "div_down_i" => {
                    assert_eq!(div_down_i(pi(a), pi(b)).unwrap(), pi(&case.y), "{label}")
                }
                "sub_no_neg" => {
                    assert_eq!(sub_no_neg(pi(a), pi(b)).unwrap(), pi(&case.y), "{label}")
                }
                "to_i256" => assert_eq!(to_i256(pu(a)).unwrap(), pi(&case.y), "{label}"),
                "to_u256" => assert_eq!(to_u256(pi(a)).unwrap(), pu(&case.y), "{label}"),
                other => panic!("unknown fixture op {other}"),
            }
        }
    }

    /// The guards that revert on chain. The port must error on the same inputs, and with the
    /// reason the guard is there for rather than merely with something.
    #[test]
    fn pmath_failures_match_the_contract() {
        let fixtures: RevertFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/pmath_reverts.json")).unwrap();
        assert!(!fixtures.reverts.is_empty());

        for case in &fixtures.reverts {
            let int_max_plus_one = U256::from_str_radix(
                "57896044618658097711785492504343953926634992332820282019728792003956564819968",
                10,
            )
            .unwrap();
            let matched = match case.case.as_str() {
                "sub_no_neg_goes_negative" => {
                    matches!(sub_no_neg(s(3), s(4)), Err(PendleError::NegativeResult { .. }))
                }
                "to_i256_above_int256_max" => {
                    matches!(to_i256(int_max_plus_one), Err(PendleError::CastOutOfRange { .. }))
                }
                "to_u256_of_negative" => {
                    matches!(to_u256(s(-1)), Err(PendleError::CastOutOfRange { .. }))
                }
                "div_down_u_by_zero" => {
                    matches!(div_down(u(1), U256::ZERO), Err(PendleError::DivisionByZero { .. }))
                }
                "div_down_i_by_zero" => {
                    matches!(div_down_i(s(1), I256::ZERO), Err(PendleError::DivisionByZero { .. }))
                }
                "raw_div_up_by_zero" => {
                    matches!(raw_div_up(u(1), U256::ZERO), Err(PendleError::DivisionByZero { .. }))
                }
                // The contract multiplies before dividing, so a product past 256 bits reverts even
                // where the quotient would have fit. The port turns that wrap into an error.
                "mul_down_u_overflows" => {
                    matches!(mul_down(U256::MAX, u(2)), Err(PendleError::Overflow { .. }))
                }
                other => panic!("unknown revert fixture {other}"),
            };
            assert!(matched, "{} did not fail the way the contract does", case.case);
        }
    }

    /// The rounding direction is the reason this module exists: `mulDown` on a product that does
    /// not divide evenly must lose the remainder, not round it up.
    ///
    /// Both operands are fixed-point, so "three" is `3e18`, not `3`. Passing the raw integer
    /// instead computes `3 * third / 1e18`, which truncates to zero — the same units mistake this
    /// module exists to prevent.
    #[test]
    fn mul_down_truncates_rather_than_rounding() {
        // 3.0 * (1/3) = 0.999999999999999999, one wei short of one.
        let third = one() / u(3);
        assert_eq!(mul_down(u(3) * one(), third).unwrap(), u(999_999_999_999_999_999));
    }

    /// A raw integer where a fixed-point value belongs is off by 1e18, and truncates to nothing.
    #[test]
    fn mul_down_of_raw_integers_underflows_to_zero() {
        let third = one() / u(3);
        assert_eq!(mul_down(u(3), third).unwrap(), U256::ZERO);
    }

    #[test]
    fn div_down_truncates_rather_than_rounding() {
        assert_eq!(div_down(u(1), u(3)).unwrap(), u(333_333_333_333_333_333));
    }

    /// `rawDivUp` rounds the other way, and is used where the protocol must not under-charge.
    #[test]
    fn raw_div_up_rounds_away_from_zero() {
        assert_eq!(raw_div_up(u(10), u(3)).unwrap(), u(4));
        assert_eq!(raw_div_up(u(9), u(3)).unwrap(), u(3));
        assert_eq!(raw_div_up(u(0), u(3)).unwrap(), u(0));
    }

    /// Signed division truncates toward zero in both languages, so a negative product keeps the
    /// same rounding as the contract.
    #[test]
    fn signed_mul_down_truncates_toward_zero() {
        let third = i_one() / s(3);
        assert_eq!(mul_down_i(s(-3) * i_one(), third).unwrap(), s(-999_999_999_999_999_999));
        assert_eq!(mul_down_i(s(3) * i_one(), third).unwrap(), s(999_999_999_999_999_999));
    }

    #[test]
    fn sub_no_neg_refuses_to_go_negative() {
        assert_eq!(sub_no_neg(s(5), s(3)).unwrap(), s(2));
        assert_eq!(sub_no_neg(s(3), s(3)).unwrap(), I256::ZERO);
        assert!(matches!(sub_no_neg(s(3), s(4)), Err(PendleError::NegativeResult { .. })));
    }

    #[test]
    fn sub_max_0_floors_at_zero() {
        assert_eq!(sub_max_0(u(5), u(3)), u(2));
        assert_eq!(sub_max_0(u(3), u(5)), U256::ZERO);
    }

    #[test]
    fn casts_are_guarded_in_both_directions() {
        assert_eq!(to_i256(u(7)).unwrap(), s(7));
        assert_eq!(to_u256(s(7)).unwrap(), u(7));
        assert!(matches!(to_u256(s(-1)), Err(PendleError::CastOutOfRange { .. })));
        assert!(matches!(to_i256(U256::MAX), Err(PendleError::CastOutOfRange { .. })));
    }

    /// The approximation loop accepts a guess only from below, within eps. Above `b` it must
    /// reject however close it is, because the contract would revert on the slippage check.
    #[test]
    fn approx_acceptance_is_one_sided() {
        let eps = u(1_000_000_000_000_000); // 1e15, the router's 0.1%
        let b = u(1_000_000_000_000_000_000);
        assert!(is_a_smaller_approx_b(b, b, eps).unwrap());
        assert!(is_a_smaller_approx_b(b - u(1), b, eps).unwrap());
        assert!(!is_a_smaller_approx_b(b + u(1), b, eps).unwrap());
        // Just inside and just outside the tolerance.
        assert!(is_a_smaller_approx_b(u(999_000_000_000_000_000), b, eps).unwrap());
        assert!(!is_a_smaller_approx_b(u(998_999_999_999_999_999), b, eps).unwrap());
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_panic() {
        assert!(matches!(div_down(u(1), U256::ZERO), Err(PendleError::DivisionByZero { .. })));
        assert!(matches!(raw_div_up(u(1), U256::ZERO), Err(PendleError::DivisionByZero { .. })));
        assert!(matches!(div_down_i(s(1), I256::ZERO), Err(PendleError::DivisionByZero { .. })));
    }
}
