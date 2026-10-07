//! A ladder of price levels, the shape every venue's book takes once its wire format is
//! decoded: filling an amount level by level, inverting a side, and valuing a side.
//!
//! Venues decode their wire levels into [`Levels`] at the ingestion boundary and store that, so
//! the simulation math runs on validated data without converting per call.

use derive_more::Deref;
use itertools::Itertools as _;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// How much of the input a fill may leave unspent and still count as complete: four ulps.
///
/// An amount reaches a fill through atomic units — `sim::to_atomic` scales it by `10^decimals`
/// and truncates it into a `BigUint`, `sim::to_human` divides it back — and each of those three
/// steps rounds, with a fourth ulp for the subtraction that measures what the walk left. Measured
/// live, the round trip alone overshoots by up to one ulp.
const FILL_TOLERANCE: f64 = 4.0 * f64::EPSILON;

/// One price level: `quantity` units of the input token, each worth `price` units of the output
/// token. Inside a [`Levels`] both are finite and positive.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: f64,
    pub quantity: f64,
}

impl PriceLevel {
    /// The one place a level is judged fit to sit in a [`Levels`]: `Some` for a level that offers
    /// something at a price, `None` for a zero-quantity placeholder (some venues publish `[0, 0]`),
    /// and `Err` for a quantity or price no trade could be read out of. What to do about the last
    /// two is the caller's: a ladder arriving from a venue rejects them, one computed from another
    /// ladder drops them.
    fn fillable(self) -> Result<Option<Self>, InvalidLevel> {
        if !self.quantity.is_finite() || self.quantity < 0.0 {
            return Err(InvalidLevel::Quantity(self.quantity));
        }
        if self.quantity == 0.0 {
            return Ok(None);
        }
        if !self.price.is_finite() || self.price <= 0.0 {
            return Err(InvalidLevel::Price(self.price));
        }
        Ok(Some(self))
    }
}

/// Why a level cannot be part of a ladder.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
pub enum InvalidLevel {
    #[error("price level quantity must be a non-negative finite number, got {0}")]
    Quantity(f64),
    #[error("price level price must be a positive finite number, got {0}")]
    Price(f64),
}

/// A ladder of price levels in the order a fill walks them, as the venue published it.
///
/// Every level has a finite positive quantity and price: [`Levels::new`] drops zero-quantity
/// placeholders and rejects everything else that is not, and deserialization goes through the
/// same check. The ladder derefs to a slice for read access.
#[derive(Clone, Debug, Default, Deref, PartialEq, Serialize)]
#[serde(transparent)]
#[deref(forward)]
pub struct Levels(Vec<PriceLevel>);

impl Levels {
    /// Builds a ladder from raw levels. Zero-quantity levels carry no liquidity and are dropped
    /// whatever their price (some venues publish `[0, 0]` placeholders). Fails on a negative or
    /// non-finite quantity, or a price that is not finite and positive on a level with quantity.
    pub fn new(levels: Vec<PriceLevel>) -> Result<Self, InvalidLevel> {
        // Collecting out of the caller's own `Vec` builds the ladder in that buffer, which a
        // deserializer grew by doubling and left with up to half of it unused; the shrink hands
        // that back, since a ladder is held for as long as the book it prices.
        let mut levels: Vec<_> = levels
            .into_iter()
            .filter_map(|level| level.fillable().transpose())
            .try_collect()?;
        levels.shrink_to_fit();
        Ok(Levels(levels))
    }

    /// Consumes `amount_in` level by level, taking as much as each level offers, until the input
    /// is spent or the ladder is exhausted. Never fails: an amount the ladder cannot absorb comes
    /// back as `remaining_in`, and an infinite one takes the whole ladder and reports the rest as
    /// unfilled.
    pub fn fill(&self, amount_in: f64) -> Fill {
        // An amount that is not a positive number buys nothing. Leaving it to the loop would rest
        // on `f64::min` returning the *other* operand for a NaN, which would walk the entire
        // ladder and report all of its output against an amount that never existed.
        if amount_in.is_nan() || amount_in <= 0.0 {
            return Fill { amount_out: 0.0, remaining_in: amount_in };
        }

        // Summing what the ladder gives and subtracting once, rather than decrementing per level:
        // a walk of the whole ladder then adds its quantities in the same order `totals` does, so
        // filling exactly what `totals` reported leaves exactly nothing.
        let mut consumed = 0.0;
        let mut amount_out = 0.0;
        for level in &self.0 {
            let taken = (amount_in - consumed).min(level.quantity);
            amount_out += taken * level.price;
            consumed += taken;
            if consumed >= amount_in {
                break;
            }
        }

        let remaining_in = amount_in - consumed;
        // An amount that came through atomic units can land a hair above what the ladder holds
        // (see `FILL_TOLERANCE`), and that hair is not liquidity anyone can trade. An infinite
        // amount keeps its infinite remainder.
        let remaining_in = if remaining_in.is_finite() && remaining_in <= amount_in * FILL_TOLERANCE
        {
            0.0
        } else {
            remaining_in
        };

        Fill { amount_out, remaining_in }
    }

    /// Re-expresses the ladder for the opposite trade direction: `(quote per base, base
    /// quantity)` becomes `(base per quote, quote quantity)`. A level whose inverse is not
    /// fillable — the price overflows to a non-finite value, or the quantity underflows to zero —
    /// is dropped.
    pub fn invert(&self) -> Levels {
        Levels(
            self.0
                .iter()
                .filter_map(|level| {
                    PriceLevel { price: 1.0 / level.price, quantity: level.quantity * level.price }
                        .fillable()
                        .ok()
                        .flatten()
                })
                .collect(),
        )
    }

    /// The ladder's value in output units: the sum of `price * quantity` over every level.
    pub fn notional(&self) -> f64 {
        self.0
            .iter()
            .map(|level| level.price * level.quantity)
            .sum()
    }

    /// Total input the ladder absorbs and the output it pays for it: the two swap limits.
    pub fn totals(&self) -> (f64, f64) {
        self.0
            .iter()
            .fold((0.0, 0.0), |(quantity, notional), level| {
                (quantity + level.quantity, notional + level.price * level.quantity)
            })
    }

    /// Average output per input unit for filling `amount_in`, priced on the part the ladder
    /// absorbs when it cannot absorb everything. `None` when nothing is consumed — an empty
    /// ladder, or an amount that is not a positive number, or one too large to subtract.
    pub fn average_price(&self, amount_in: f64) -> Option<f64> {
        let fill = self.fill(amount_in);
        let consumed = amount_in - fill.remaining_in;
        (consumed > 0.0).then(|| fill.amount_out / consumed)
    }
}

impl<'de> Deserialize<'de> for Levels {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Levels::new(Vec::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// What filling an amount against a ladder yields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    /// Output bought by the consumed input.
    pub amount_out: f64,
    /// Input the ladder could not absorb; zero when the fill is complete.
    pub remaining_in: f64,
}

impl Fill {
    pub fn is_complete(&self) -> bool {
        self.remaining_in <= 0.0
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn ladder() -> Levels {
        Levels::new(vec![
            PriceLevel { price: 3000.0, quantity: 1.0 },
            PriceLevel { price: 2999.0, quantity: 2.0 },
        ])
        .unwrap()
    }

    #[test]
    fn new_drops_zero_quantity_placeholders() {
        let levels = Levels::new(vec![
            PriceLevel { price: 0.0, quantity: 0.0 },
            PriceLevel { price: 3000.0, quantity: 1.0 },
            PriceLevel { price: -1.0, quantity: 0.0 },
        ])
        .unwrap();

        assert_eq!(&*levels, &[PriceLevel { price: 3000.0, quantity: 1.0 }]);
    }

    #[test]
    fn new_rejects_invalid_levels() {
        assert_eq!(
            Levels::new(vec![PriceLevel { price: 1.0, quantity: -1.0 }]),
            Err(InvalidLevel::Quantity(-1.0))
        );
        assert!(matches!(
            Levels::new(vec![PriceLevel { price: 1.0, quantity: f64::NAN }]),
            Err(InvalidLevel::Quantity(_))
        ));
        assert_eq!(
            Levels::new(vec![PriceLevel { price: 0.0, quantity: 1.0 }]),
            Err(InvalidLevel::Price(0.0))
        );
        assert_eq!(
            Levels::new(vec![PriceLevel { price: f64::INFINITY, quantity: 1.0 }]),
            Err(InvalidLevel::Price(f64::INFINITY))
        );
    }

    #[test]
    fn deserialize_validates_and_serialize_is_transparent() {
        let levels: Levels = serde_json::from_str(
            r#"[{"price":3000.0,"quantity":1.0},{"price":1.0,"quantity":0.0}]"#,
        )
        .unwrap();
        assert_eq!(levels.len(), 1);
        assert_eq!(serde_json::to_string(&levels).unwrap(), r#"[{"price":3000.0,"quantity":1.0}]"#);

        assert!(serde_json::from_str::<Levels>(r#"[{"price":0.0,"quantity":1.0}]"#).is_err());
    }

    #[test]
    fn fill_walks_levels_in_order_and_reports_the_unfilled_rest() {
        let ladder = ladder();

        assert_eq!(ladder.fill(1.0), Fill { amount_out: 3000.0, remaining_in: 0.0 });
        assert_eq!(ladder.fill(2.0), Fill { amount_out: 5999.0, remaining_in: 0.0 });
        let partial = ladder.fill(5.0);
        assert_eq!(partial, Fill { amount_out: 8998.0, remaining_in: 2.0 });
        assert!(!partial.is_complete());
    }

    #[rstest]
    #[case::nothing(0.0)]
    #[case::less_than_nothing(-1.0)]
    #[case::not_a_number(f64::NAN)]
    fn fill_of_an_amount_that_is_not_positive_takes_nothing(#[case] amount_in: f64) {
        // Least obvious for the NaN: `f64::min` answers with its other operand, so without a
        // guard every level would report itself fully taken and the ladder would pay out 8998 (1 ×
        // 3000 + 2 × 2999).
        let fill = ladder().fill(amount_in);

        assert_eq!(fill.amount_out, 0.0);
        assert!(
            fill.remaining_in.to_bits() == amount_in.to_bits(),
            "the amount comes back untouched"
        );
    }

    #[test]
    fn fill_of_an_infinite_amount_takes_the_whole_ladder() {
        // What an amount past `f64::MAX` becomes on its way in: the ladder pays everything it
        // has and the fill reports itself incomplete, rather than claiming to have filled it.
        let fill = ladder().fill(f64::INFINITY);

        assert_eq!(fill.amount_out, 8998.0);
        assert_eq!(fill.remaining_in, f64::INFINITY);
        assert!(!fill.is_complete());
    }

    /// Ten levels whose quantities sum to 98.008172. Subtracting them from that sum one at a
    /// time leaves 7.1e-15 behind — the sum and the subtractions round differently — which used
    /// to make a swap of exactly the ladder's depth report itself unfilled.
    #[test]
    fn filling_the_whole_ladder_leaves_nothing() {
        let quantities = [1.1, 2.2, 3.3, 4.4, 5.5, 6.6, 7.7, 8.8, 9.9, 48.508172];
        let ladder = Levels::new(
            quantities
                .iter()
                .map(|&quantity| PriceLevel { price: 2.0, quantity })
                .collect(),
        )
        .unwrap();
        let (depth, _) = ladder.totals();

        let fill = ladder.fill(depth);

        assert_eq!(fill.remaining_in, 0.0);
        assert!(fill.is_complete());
    }

    /// The tolerance covers rounding, not liquidity: a ladder asked for a thousandth more than it
    /// holds still reports that thousandth as unfilled.
    #[test]
    fn filling_past_the_ladder_still_reports_the_shortfall() {
        let ladder = ladder();
        let (depth, _) = ladder.totals();

        let fill = ladder.fill(depth + 0.001);

        assert!((fill.remaining_in - 0.001).abs() < 1e-12, "got {}", fill.remaining_in);
        assert!(!fill.is_complete());
    }

    #[test]
    fn invert_swaps_units_and_drops_overflowing_levels() {
        // (0.11 TAMARA per USDC, 3000 USDC) becomes (9.09 USDC per TAMARA, 330 TAMARA).
        let inverted = Levels::new(vec![
            PriceLevel { price: 0.11, quantity: 3000.0 },
            PriceLevel { price: 0.12, quantity: 3000.0 },
            PriceLevel { price: 1e308, quantity: 1e308 },
        ])
        .unwrap()
        .invert();

        assert_eq!(inverted.len(), 2);
        assert!((inverted[0].price - 9.090909090909092).abs() < 1e-9);
        assert!((inverted[0].quantity - 330.0).abs() < 1e-9);
        assert!((inverted[1].price - 8.333333333333334).abs() < 1e-9);
        assert!((inverted[1].quantity - 360.0).abs() < 1e-9);
    }

    #[test]
    fn notional_and_totals_sum_the_ladder() {
        let ladder = ladder();

        assert_eq!(ladder.notional(), 8998.0);
        assert_eq!(ladder.totals(), (3.0, 8998.0));
    }

    #[test]
    fn average_price_is_per_consumed_unit() {
        let ladder = ladder();

        assert_eq!(ladder.average_price(1.0), Some(3000.0));
        assert_eq!(ladder.average_price(2.0), Some(2999.5));
        // Only 3 of 5 units fill; the price is over those 3.
        assert_eq!(ladder.average_price(5.0), Some(8998.0 / 3.0));
        assert_eq!(ladder.average_price(0.0), None);
        assert_eq!(Levels::default().average_price(1.0), None);
    }
}
