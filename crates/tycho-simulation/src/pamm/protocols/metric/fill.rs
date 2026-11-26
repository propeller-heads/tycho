//! The exact fill inside one depth bin, as Metric's "Estimate a fill from depth" page gives it.
//!
//! A bin's price moves in a straight line with the output it pays, from an opening price to the
//! closing `price` of its depth level. The proportional share of a bin is a lower bound on what an
//! order that ends inside it receives; this module prices the part the order takes along that line.
//! Every bin before the one the order ends in is crossed whole, by the caller.

use num_bigint::BigUint;
use num_traits::{One, Zero};

use crate::book::sim;

/// The side of the book an order walks: bids sell token0, asks buy it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BookSide {
    Bids,
    Asks,
}

/// One depth bin: the input and output it adds, and its closing price (Q64.64).
#[derive(Clone, Copy)]
pub(super) struct Bin<'a> {
    pub(super) input: &'a BigUint,
    pub(super) output: &'a BigUint,
    pub(super) price: &'a BigUint,
}

/// The output of the part of `bin` that `remaining` input buys, from the bin's own curve, or `None`
/// when the depth does not describe a straight-line bin and the proportional share applies.
///
/// `decimals` are the token0 and token1 decimals. `remaining` must be less than the bin's input.
pub(super) fn exact_fill_inside_bin(
    bin: Bin<'_>,
    side: BookSide,
    remaining: &BigUint,
    decimals: (u32, u32),
) -> Option<BigUint> {
    if remaining.is_zero() {
        return Some(BigUint::ZERO);
    }
    let extra = bin_extra_input(bin, side, decimals)?;
    let low = lowest_price(bin, side, &extra)?;
    let estimate = match side {
        BookSide::Asks => output_for_input_on_asks(bin, &extra, remaining),
        BookSide::Bids => output_for_input_on_bids(bin, &extra, remaining),
    };
    let margin = margin(&estimate, &low);
    let margined = if estimate > margin { estimate - margin } else { BigUint::ZERO };
    let proportional_floor = bin.output * remaining / bin.input;
    Some(
        margined
            .max(proportional_floor)
            .min(bin.output.clone()),
    )
}

/// The price of the next unit once `remaining` input is taken inside `bin`, along the bin's own
/// curve: output per input on bids, input per output on asks, in human units. `None` when the bin
/// has no curve, and the caller uses the bin's average price.
///
/// On asks `exact_fill_inside_bin` solves `(I - E)·t + E·t² = in` for `t = x / O`, so the price
/// is the slope `((I - E) + 2·E·t) / O`. On bids the fill is the walk's guess
/// `G = O·in / (I - E)` run through one correction, `G·in / C(G)`, so the price is the slope of
/// that expression. The walk applies the correction only when `C(G)` exceeds the input; otherwise
/// the price is `O / (I - E)`.
pub(super) fn marginal_price_inside_bin(
    bin: Bin<'_>,
    side: BookSide,
    remaining: &BigUint,
    decimals: (u32, u32),
) -> Option<f64> {
    let extra = bin_extra_input(bin, side, decimals)?;
    let (input_decimals, output_decimals) = match side {
        BookSide::Bids => (decimals.0, decimals.1),
        BookSide::Asks => (decimals.1, decimals.0),
    };
    let input = sim::to_human(bin.input, input_decimals);
    let extra = sim::to_human(&extra, input_decimals);
    let output = sim::to_human(bin.output, output_decimals);
    match side {
        BookSide::Asks => {
            let taken = exact_fill_inside_bin(bin, side, remaining, decimals)?;
            let fraction = sim::to_human(&taken, output_decimals) / output;
            Some(((input - extra) + 2.0 * extra * fraction) / output)
        }
        BookSide::Bids => {
            let remaining = sim::to_human(remaining, input_decimals);
            bid_marginal_price(input, extra, output, remaining)
        }
    }
}

/// The slope of the walk's bid fill at `remaining` input, from the guess and its one correction.
/// `None` when the guess reaches the bin's whole output, where the walk clamps it.
fn bid_marginal_price(input: f64, extra: f64, output: f64, remaining: f64) -> Option<f64> {
    let opening_slope = output / (input - extra);
    let guess = opening_slope * remaining;
    if guess >= output {
        return None;
    }
    let cost = bid_input_for_guess(input, extra, output, guess);
    if cost <= remaining {
        return Some(opening_slope);
    }
    let cost_slope = bid_input_slope(input, extra, output, guess);
    Some(
        (opening_slope * remaining + guess) / cost -
            remaining * guess * cost_slope * opening_slope / (cost * cost),
    )
}

/// The input the pool charges for `output` of the bin on bids, in human units. The continuous form
/// of `bid_input_for_output`.
fn bid_input_for_guess(input: f64, extra: f64, bin_output: f64, output: f64) -> f64 {
    let net = input - extra;
    let high_output = (input + extra) * bin_output;
    output * net * (high_output - extra * output) /
        (bin_output * (high_output - 2.0 * extra * output))
}

/// The derivative of `bid_input_for_guess` in `output`.
fn bid_input_slope(input: f64, extra: f64, bin_output: f64, output: f64) -> f64 {
    let net = input - extra;
    let high_output = (input + extra) * bin_output;
    let curvature = high_output * high_output - 2.0 * extra * high_output * output +
        2.0 * extra * extra * output * output;
    let slope = high_output - 2.0 * extra * output;
    net * curvature / (bin_output * slope * slope)
}

/// `E`: what the whole bin would cost at its closing price, minus what it costs, the bin's
/// curvature. `None` when `E` is not positive, or is too large for a straight-line bin of that
/// side.
fn bin_extra_input(bin: Bin<'_>, side: BookSide, decimals: (u32, u32)) -> Option<BigUint> {
    let closing = closing_input(bin, side, decimals)?;
    let input = bin.input;
    if closing <= *input {
        return None;
    }
    let two_inputs = input * 2u8;
    let in_range = match side {
        BookSide::Asks => closing <= two_inputs,
        BookSide::Bids => closing < two_inputs,
    };
    in_range.then(|| closing - input)
}

/// What the whole bin would cost at its closing price, in input raw units.
fn closing_input(
    bin: Bin<'_>,
    side: BookSide,
    (decimals0, decimals1): (u32, u32),
) -> Option<BigUint> {
    let fixed_point = BigUint::one() << 64usize;
    let scale0 = BigUint::from(10u8).pow(decimals0);
    let scale1 = BigUint::from(10u8).pow(decimals1);
    match side {
        BookSide::Asks => Some(bin.output * bin.price * &scale1 / (fixed_point * scale0)),
        BookSide::Bids => {
            let price_in_token1 = bin.price * &scale1;
            if price_in_token1.is_zero() {
                return None;
            }
            Some(bin.output * fixed_point * scale0 / price_in_token1)
        }
    }
}

/// The lowest price in the bin, which sizes the rounding margin: the opening price on asks, where
/// the price rises through the bin, and the closing price on bids, where it falls.
fn lowest_price(bin: Bin<'_>, side: BookSide, extra: &BigUint) -> Option<BigUint> {
    let low = match side {
        BookSide::Asks => bin.price * (bin.input - extra) / (bin.input + extra),
        BookSide::Bids => bin.price.clone(),
    };
    (!low.is_zero()).then_some(low)
}

/// The margin Metric's own rounding can move an estimate by: two grid steps at the lowest price,
/// plus one unit.
fn margin(estimate: &BigUint, low: &BigUint) -> BigUint {
    ((estimate * 2u8 + low - BigUint::one()) / low) + BigUint::one()
}

/// Exact input on asks: solves `(I - E)·x + E·x² = r` for the output `x` with 64 fractional bits,
/// rounding the root up, then floors the result.
fn output_for_input_on_asks(bin: Bin<'_>, extra: &BigUint, remaining: &BigUint) -> BigUint {
    let scaled = (bin.input - extra) << 64usize;
    let root = ceil_sqrt(&(&scaled * &scaled + ((extra * remaining) << 130usize)));
    ((remaining << 65usize) * bin.output) / (scaled + root)
}

/// Exact input on bids: fills at the opening price, then corrects once against the cost the pool
/// charges for that output, as the pool does.
fn output_for_input_on_bids(bin: Bin<'_>, extra: &BigUint, remaining: &BigUint) -> BigUint {
    let guess = ((bin.output * remaining) / (bin.input - extra)).min(bin.output.clone());
    let cost = bid_input_for_output(bin, extra, &guess);
    if cost <= *remaining {
        guess
    } else {
        guess * remaining / cost
    }
}

/// The input the pool charges for `output` of the bin on bids, rounded up.
fn bid_input_for_output(bin: Bin<'_>, extra: &BigUint, output: &BigUint) -> BigUint {
    let net = bin.input - extra;
    let high = bin.input + extra;
    let numerator = output * &net * (&high * bin.output - extra * output);
    let denominator = bin.output * (&high * bin.output - extra * output * 2u8);
    if denominator.is_zero() {
        return BigUint::ZERO;
    }
    (numerator + &denominator - BigUint::one()) / denominator
}

/// The smallest integer whose square is at least `value`.
fn ceil_sqrt(value: &BigUint) -> BigUint {
    let root = value.sqrt();
    if &root * &root < *value {
        root + BigUint::one()
    } else {
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(value: u128) -> BigUint {
        BigUint::from(value)
    }

    /// An ask bin that pays 100 000 output for 100 000 input, closing at 1.25 (exact in Q64.64).
    /// Its price opens at 0.75, so the first 50 000 of output cost 0.875 each on average: 43 750.
    /// The estimate is exactly 50 000 before the two-unit margin.
    #[test]
    fn asks_bin_is_priced_along_its_straight_line() {
        let input = fixed(100_000);
        let output = fixed(100_000);
        let closing = fixed(5) << 62usize;
        let bin = Bin { input: &input, output: &output, price: &closing };

        let inside = exact_fill_inside_bin(bin, BookSide::Asks, &fixed(43_750), (0, 0));

        assert_eq!(inside, Some(fixed(49_998)));
    }

    /// A bin whose closing price implies no extra cost over its input has no curve to follow, so
    /// the caller falls back to the proportional share.
    #[test]
    fn flat_bin_has_no_exact_fill() {
        let input = fixed(100_000);
        let output = fixed(100_000);
        let flat = BigUint::one() << 64usize;
        let bin = Bin { input: &input, output: &output, price: &flat };

        assert_eq!(exact_fill_inside_bin(bin, BookSide::Asks, &fixed(47_500), (0, 0)), None);
    }
}
