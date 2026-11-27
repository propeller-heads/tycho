//! Spot prices and the fee: [`FiniteSpotPrice`], [`FeeFraction`], [`SpotPriceWhereTradable`],
//! [`SpotPricesProductAtLeastOne`], [`QuoteAtLeastSpotBefore`] and [`QuoteAtMostSpotAfter`].
//!
//! For selling A for B, `q(x)` is `get_amount_out(x, A, B)` and `spot(B, A)` is
//! `spot_price(base = B, quote = A)`: how much A one unit of B costs, fee included, as
//! `spot_price` documents.

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use tycho_simulation::tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim};

use super::{finite_positive, Check, Direction, Limit, PropertyViolations, Quotes, Sale};

/// Float rounding allowed where a property compares spot prices, in log10: about 2.3e-9
/// relative. The prices are computed in `f64` from integer state, so they carry relative errors
/// of a few ulps, far below this.
const LOG10_TOLERANCE: f64 = 1e-9;

/// A spot price that `spot_price` returns is finite and positive. The check returns
/// `spot(buy, sell)`, the price of buying in this direction, when it is one; whether an error is
/// allowed is [`SpotPriceWhereTradable`]'s to say.
pub struct FiniteSpotPrice<'a> {
    pub state: &'a dyn ProtocolSim,
    /// Names the state in the violations.
    pub subject: &'a str,
    pub direction: &'a Direction<'a>,
}

impl Check for FiniteSpotPrice<'_> {
    const NAME: &'static str = "finite_spot_price";
    type Output = Option<f64>;

    fn check(self, violations: &mut PropertyViolations<'_>) -> Option<f64> {
        let Self { state, subject, direction } = self;
        let spot = state
            .spot_price(direction.token_out, direction.token_in)
            .ok()?;
        if !finite_positive(spot) {
            violations.push(format!(
                "{direction}: the spot price of {subject} is {spot}, not finite and positive"
            ));
            return None;
        }
        Some(spot)
    }
}

/// The fee is a fraction from 0 to 1.
pub struct FeeFraction<'a> {
    pub state: &'a dyn ProtocolSim,
    /// Names the state in the violations.
    pub subject: &'a str,
}

impl Check for FeeFraction<'_> {
    const NAME: &'static str = "fee_fraction";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { state, subject } = self;
        let fee = state.fee();
        if !(fee.is_finite() && (0.0..=1.0).contains(&fee)) {
            violations.push(format!("the fee of {subject} is {fee}, not from 0 to 1"));
        }
    }
}

/// A direction with a non-zero `max_input` has a spot price, unless its price lies beyond what an
/// `f64` holds.
pub struct SpotPriceWhereTradable<'a> {
    pub direction: &'a Direction<'a>,
    pub limit: &'a Limit,
    /// What [`FiniteSpotPrice`] handed back.
    pub spot: Option<f64>,
    pub quotes: &'a Quotes,
}

impl Check for SpotPriceWhereTradable<'_> {
    const NAME: &'static str = "spot_price_where_tradable";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, limit, spot, quotes } = self;
        if !limit.max_input.is_zero() && spot.is_none() && !price_beyond_f64(direction, quotes) {
            violations.push(format!(
                "{direction}: max_input is {} but spot_price errors",
                limit.max_input
            ));
        }
    }
}

/// Whether this direction's price lies beyond what an `f64` holds. Estimated from the smallest
/// quote that buys something: its average price is at least the spot price, so an average below
/// 10^-300 puts the spot price at the edge of the `f64` range or below it; an average above
/// 10^300 is too close to the top of the range to tell.
fn price_beyond_f64(direction: &Direction<'_>, quotes: &Quotes) -> bool {
    quotes
        .iter()
        .find(|(_, quote)| !quote.amount.is_zero())
        .and_then(|(amount, quote)| log10_price(direction, amount, &quote.amount))
        .is_some_and(|log10| log10.abs() > 300.0)
}

/// The spot prices of the two directions between `a` and `b` multiply to at least 1.
pub struct SpotPricesProductAtLeastOne<'a> {
    pub state: &'a dyn ProtocolSim,
    pub a: &'a Token,
    pub b: &'a Token,
}

impl Check for SpotPricesProductAtLeastOne<'_> {
    const NAME: &'static str = "spot_prices_product_at_least_one";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { state, a, b } = self;
        let (Ok(a_in_b), Ok(b_in_a)) = (state.spot_price(a, b), state.spot_price(b, a)) else {
            return;
        };
        if !(finite_positive(a_in_b) && finite_positive(b_in_a)) {
            // `FiniteSpotPrice` reports these.
            return;
        }
        if a_in_b.log10() + b_in_a.log10() < -LOG10_TOLERANCE {
            violations.push(format!(
                "{} and {}: the spot prices {a_in_b} and {b_in_a} multiply to {}, less than 1",
                a.address,
                b.address,
                a_in_b * b_in_a
            ));
        }
    }
}

/// The log10 of the price a quote pays per unit of output, in the tokens' units: of `bought` for
/// `paid`. Infinite when `bought` is zero; `None` past what an `f64` holds.
fn log10_price(direction: &Direction<'_>, paid: &BigUint, bought: &BigUint) -> Option<f64> {
    let decimals_shift =
        f64::from(direction.token_out.decimals) - f64::from(direction.token_in.decimals);
    Some(paid.to_f64()?.log10() - bought.to_f64()?.log10() + decimals_shift)
}

/// A quote pays at least the spot price before it: `x / q(x) >= spot_s(B, A)`. Compared in log10,
/// so no decimals overflow a float.
pub struct QuoteAtLeastSpotBefore<'a> {
    pub direction: &'a Direction<'a>,
    /// What [`FiniteSpotPrice`] handed back.
    pub spot_before: f64,
    pub quotes: &'a Quotes,
}

impl Check for QuoteAtLeastSpotBefore<'_> {
    const NAME: &'static str = "quote_at_least_spot_before";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, spot_before, quotes } = self;
        for (amount, quote) in quotes {
            // Infinite when the quote buys nothing, which no spot price exceeds.
            let Some(price) = log10_price(direction, amount, &quote.amount) else { continue };
            if spot_before.log10() > price + LOG10_TOLERANCE {
                violations.push(format!(
                    "{direction}: selling {amount} for {} pays 10^{price} per unit, less than \
                     the spot price of {spot_before} before it",
                    quote.amount
                ));
            }
        }
    }
}

/// A quote pays at most the spot price after it, with one unit of output rounding:
/// `x / (q(x) + 1) <= spot_s'(B, A)`. The rounding unit makes it hold for a quote that buys
/// nothing too. Compared in log10, so no decimals overflow a float.
pub struct QuoteAtMostSpotAfter<'a> {
    pub direction: &'a Direction<'a>,
    pub sales: &'a [Sale<'a>],
}

impl Check for QuoteAtMostSpotAfter<'_> {
    const NAME: &'static str = "quote_at_most_spot_after";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, sales } = self;
        for Sale { amount, quote, spot_after, .. } in sales {
            let Some(price) = log10_price(direction, amount, &(&quote.amount + 1u8)) else {
                continue;
            };
            // `FiniteSpotPrice` reports a spot price that errs or is not finite and positive.
            let Some(spot_after) = spot_after else { continue };
            if spot_after.log10() + LOG10_TOLERANCE < price {
                violations.push(format!(
                    "{direction}: selling {amount} for {} pays 10^{price} per unit with one unit \
                     of rounding, more than the spot price of {spot_after} after it",
                    quote.amount
                ));
            }
        }
    }
}
