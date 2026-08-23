//! Port of `LogExpMath.sol` — fixed-point `ln` and `exp` in 18 decimals.
//!
//! Provenance and licensing: see `../NOTICE.md`.
//!
//! The port follows the Solidity statement by statement, including the order of operations, so
//! that every truncation lands in the same place. Solidity's integer division truncates toward
//! zero and so does Rust's, which is what makes a literal transcription bit-exact rather than
//! merely close.
//!
//! Two deliberate departures from the original:
//!
//! - The Solidity runs `unchecked`, so an out-of-domain argument wraps silently. Here every
//!   multiplication is checked and overflow is an error. Inside the domain the contract accepts,
//!   nothing overflows, so this changes no in-domain result — it only turns a silent wrap into a
//!   loud failure.
//! - `require` becomes `Err`, since a quote must never panic.
//!
//! Bit-equality is asserted against fixtures generated from the Solidity itself; see
//! `../differential/`.

use alloy::primitives::I256;

use super::errors::{PendleError, PendleResult};

/// One, in the 18-decimal fixed point the public functions speak.
pub const ONE_18: i128 = 1_000_000_000_000_000_000;

fn i(value: i128) -> I256 {
    I256::try_from(value).expect("i128 always fits in I256")
}

/// Parses one of the wide decimal constants the decomposition needs.
fn c(literal: &str) -> I256 {
    I256::from_dec_str(literal).expect("constant is a valid decimal literal")
}

fn one_18() -> I256 {
    i(ONE_18)
}

fn one_20() -> I256 {
    i(100_000_000_000_000_000_000)
}

fn one_36() -> I256 {
    c("1000000000000000000000000000000000000")
}

/// `exp` is only defined where its 20-decimal intermediate fits in 256 bits; the contract keeps a
/// margin and uses these round numbers.
fn max_natural_exponent() -> I256 {
    i(130 * ONE_18)
}

fn min_natural_exponent() -> I256 {
    i(-41 * ONE_18)
}

/// `ln` switches to the 36-decimal path strictly inside this window.
fn ln_36_lower_bound() -> I256 {
    i(ONE_18 - 100_000_000_000_000_000)
}

fn ln_36_upper_bound() -> I256 {
    i(ONE_18 + 100_000_000_000_000_000)
}

/// The powers of two `x_n = 2^(7-n)` that `exp` decomposes its argument into, and their
/// exponentials `a_n = e^(x_n)`.
///
/// `x0`/`x1` and `a0`/`a1` are 18-decimal and 0-decimal respectively; everything from `x2` on is
/// 20-decimal. That split is why the two are handled separately below rather than in one loop.
struct Decomposition {
    x: [I256; 12],
    a: [I256; 12],
}

fn decomposition() -> Decomposition {
    Decomposition {
        x: [
            c("128000000000000000000"),
            c("64000000000000000000"),
            c("3200000000000000000000"),
            c("1600000000000000000000"),
            c("800000000000000000000"),
            c("400000000000000000000"),
            c("200000000000000000000"),
            c("100000000000000000000"),
            c("50000000000000000000"),
            c("25000000000000000000"),
            c("12500000000000000000"),
            c("6250000000000000000"),
        ],
        a: [
            c("38877084059945950922200000000000000000000000000000000000"),
            c("6235149080811616882910000000"),
            c("7896296018268069516100000000000000"),
            c("888611052050787263676000000"),
            c("298095798704172827474000"),
            c("5459815003314423907810"),
            c("738905609893065022723"),
            c("271828182845904523536"),
            c("164872127070012814685"),
            c("128402541668774148407"),
            c("113314845306682631683"),
            c("106449445891785942956"),
        ],
    }
}

fn mul(a: I256, b: I256, op: &'static str) -> PendleResult<I256> {
    a.checked_mul(b)
        .ok_or(PendleError::Overflow { operation: op })
}

fn div(a: I256, b: I256, op: &'static str) -> PendleResult<I256> {
    if b.is_zero() {
        return Err(PendleError::DivisionByZero { operation: op });
    }
    a.checked_div(b)
        .ok_or(PendleError::Overflow { operation: op })
}

/// `e^x` for an 18-decimal fixed-point `x`.
///
/// Errors outside `[-41e18, 130e18]`, where the Solidity reverts with `"Invalid exponent"`.
pub fn exp(x: I256) -> PendleResult<I256> {
    if x < min_natural_exponent() || x > max_natural_exponent() {
        return Err(PendleError::ExponentOutOfBounds { x: x.to_string() });
    }
    if x.is_negative() {
        // e^(-x) = 1 / e^x. Safe to negate: x is above MIN_NATURAL_EXPONENT, so it fits.
        let positive = exp(-x)?;
        return div(mul(one_18(), one_18(), "exp reciprocal")?, positive, "exp reciprocal");
    }

    let d = decomposition();
    let mut x = x;

    // a0 and a1 are too large to hold as 18-decimal numbers, so they are carried as plain
    // integers and folded back in at the very end.
    let first_an = if x >= d.x[0] {
        x -= d.x[0];
        d.a[0]
    } else if x >= d.x[1] {
        x -= d.x[1];
        d.a[1]
    } else {
        I256::ONE
    };

    // From here on the arithmetic is 20-decimal, for precision on the smaller terms.
    x = mul(x, i(100), "exp widen")?;

    let mut product = one_20();
    for n in 2..10 {
        if x >= d.x[n] {
            x -= d.x[n];
            product = div(mul(product, d.a[n], "exp product")?, one_20(), "exp product")?;
        }
    }
    // x10 and x11 are not used here: the remainder is already small enough for the series below to
    // reach 18-decimal precision.

    // e^x for the remainder, by Taylor series: 1 + x + x^2/2! + ... Twelve terms suffice.
    let mut series_sum = one_20();
    let mut term = x;
    series_sum += term;
    for n in 2..=12i128 {
        term = div(div(mul(term, x, "exp series")?, one_20(), "exp series")?, i(n), "exp series")?;
        series_sum += term;
    }

    let combined = div(mul(product, series_sum, "exp combine")?, one_20(), "exp combine")?;
    div(mul(combined, first_an, "exp combine")?, i(100), "exp combine")
}

/// `ln(a)` for an 18-decimal fixed-point `a`.
///
/// Errors at or below zero, where the Solidity reverts with `"out of bounds"`.
pub fn ln(a: I256) -> PendleResult<I256> {
    if a <= I256::ZERO {
        return Err(PendleError::LogarithmOutOfBounds { a: a.to_string() });
    }
    if ln_36_lower_bound() < a && a < ln_36_upper_bound() {
        // Close to one, where the result is small enough that 36 decimals are worth carrying.
        return div(ln_36(a)?, one_18(), "ln_36 narrow");
    }
    ln_internal(a)
}

fn ln_internal(a: I256) -> PendleResult<I256> {
    if a < one_18() {
        // ln(a) = -ln(1/a). The reciprocal is above one, so this recurses at most once.
        let reciprocal = div(mul(one_18(), one_18(), "ln reciprocal")?, a, "ln reciprocal")?;
        return Ok(-ln_internal(reciprocal)?);
    }

    let d = decomposition();
    let mut a = a;
    let mut sum = I256::ZERO;

    // a0 and a1 are 0-decimal, so they are compared against `a_n * ONE_18` and divided out as
    // plain integers.
    for n in 0..2 {
        if a >= mul(d.a[n], one_18(), "ln decompose")? {
            a = div(a, d.a[n], "ln decompose")?;
            sum += d.x[n];
        }
    }

    // Everything below is 20-decimal.
    sum = mul(sum, i(100), "ln widen")?;
    a = mul(a, i(100), "ln widen")?;

    for n in 2..12 {
        if a >= d.a[n] {
            a = div(mul(a, one_20(), "ln decompose")?, d.a[n], "ln decompose")?;
            sum += d.x[n];
        }
    }

    // The remainder is now near one, so ln converges fast as
    // 2 * (z + z^3/3 + z^5/5 + ...) with z = (a - 1) / (a + 1).
    let z = div(mul(a - one_20(), one_20(), "ln series")?, a + one_20(), "ln series")?;
    let z_squared = div(mul(z, z, "ln series")?, one_20(), "ln series")?;

    let mut num = z;
    let mut series_sum = num;
    for divisor in [3i128, 5, 7, 9, 11] {
        num = div(mul(num, z_squared, "ln series")?, one_20(), "ln series")?;
        series_sum += div(num, i(divisor), "ln series")?;
    }
    series_sum = mul(series_sum, i(2), "ln series")?;

    div(sum + series_sum, i(100), "ln narrow")
}

/// `ln(x)` in 36 decimals, for `x` within a tenth of one.
fn ln_36(x: I256) -> PendleResult<I256> {
    let x = mul(x, one_18(), "ln_36 widen")?;

    let z = div(mul(x - one_36(), one_36(), "ln_36 series")?, x + one_36(), "ln_36 series")?;
    let z_squared = div(mul(z, z, "ln_36 series")?, one_36(), "ln_36 series")?;

    let mut num = z;
    let mut series_sum = num;
    for divisor in [3i128, 5, 7, 9, 11, 13, 15] {
        num = div(mul(num, z_squared, "ln_36 series")?, one_36(), "ln_36 series")?;
        series_sum += div(num, i(divisor), "ln_36 series")?;
    }

    mul(series_sum, i(2), "ln_36 series")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct ExpCase {
        x: String,
        y: String,
    }

    #[derive(Deserialize)]
    struct LnCase {
        a: String,
        y: String,
    }

    #[derive(Deserialize)]
    struct ExpFixtures {
        exp: Vec<ExpCase>,
    }

    #[derive(Deserialize)]
    struct LnFixtures {
        ln: Vec<LnCase>,
    }

    fn parse(value: &str) -> I256 {
        I256::from_dec_str(value).expect("fixture holds a decimal integer")
    }

    /// Bit-equality against values produced by the Solidity itself. The grid probes every
    /// decomposition boundary exactly and either side of it, which is where an off-by-one in the
    /// `>=` branches would show up.
    #[test]
    fn exp_matches_the_contract() {
        let fixtures: ExpFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/exp.json")).unwrap();
        assert!(fixtures.exp.len() >= 80, "fixture grid shrank unexpectedly");
        for case in fixtures.exp {
            let x = parse(&case.x);
            assert_eq!(
                exp(x).unwrap_or_else(|e| panic!("exp({}) failed: {e}", case.x)),
                parse(&case.y),
                "exp({})",
                case.x
            );
        }
    }

    #[test]
    fn ln_matches_the_contract() {
        let fixtures: LnFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/ln.json")).unwrap();
        assert!(fixtures.ln.len() >= 20, "fixture grid shrank unexpectedly");
        for case in fixtures.ln {
            let a = parse(&case.a);
            assert_eq!(
                ln(a).unwrap_or_else(|e| panic!("ln({}) failed: {e}", case.a)),
                parse(&case.y),
                "ln({})",
                case.a
            );
        }
    }

    /// The contract reverts here, so the port must error rather than return a plausible number.
    #[test]
    fn exp_rejects_arguments_outside_its_domain() {
        assert!(matches!(
            exp(max_natural_exponent() + I256::ONE),
            Err(PendleError::ExponentOutOfBounds { .. })
        ));
        assert!(matches!(
            exp(min_natural_exponent() - I256::ONE),
            Err(PendleError::ExponentOutOfBounds { .. })
        ));
        assert!(exp(max_natural_exponent()).is_ok());
        assert!(exp(min_natural_exponent()).is_ok());
    }

    #[test]
    fn ln_rejects_arguments_at_or_below_zero() {
        assert!(matches!(ln(I256::ZERO), Err(PendleError::LogarithmOutOfBounds { .. })));
        assert!(matches!(ln(-I256::ONE), Err(PendleError::LogarithmOutOfBounds { .. })));
        assert!(ln(I256::ONE).is_ok());
    }

    /// The identity the AMM leans on hardest: the implied rate is stored as a logarithm and
    /// exponentiated back on every trade.
    #[test]
    fn exp_and_ln_round_trip_at_one() {
        assert_eq!(ln(one_18()).unwrap(), I256::ZERO);
        assert_eq!(exp(I256::ZERO).unwrap(), one_18());
    }
}
