//! A ladder of price levels, the shape every venue's book takes once its wire format is
//! decoded: filling an amount level by level, inverting a side, and valuing a side.
//!
//! Venues decode their wire levels into [`Levels`] at the ingestion boundary and store that, so
//! the simulation math runs on validated data without converting per call.

use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// One price level: `quantity` units of the input token, each worth `price` units of the output
/// token. Inside a [`Levels`] both are finite and positive.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: f64,
    pub quantity: f64,
}

impl PriceLevel {
    fn is_valid(&self) -> bool {
        self.quantity.is_finite() &&
            self.quantity > 0.0 &&
            self.price.is_finite() &&
            self.price > 0.0
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Levels(Vec<PriceLevel>);

impl Levels {
    /// Builds a ladder from raw levels. Zero-quantity levels carry no liquidity and are dropped
    /// whatever their price (some venues publish `[0, 0]` placeholders). Fails on a negative or
    /// non-finite quantity, or a price that is not finite and positive on a level with quantity.
    pub fn new(levels: Vec<PriceLevel>) -> Result<Self, InvalidLevel> {
        let mut valid = Vec::with_capacity(levels.len());
        for level in levels {
            if !level.quantity.is_finite() || level.quantity < 0.0 {
                return Err(InvalidLevel::Quantity(level.quantity));
            }
            if level.quantity == 0.0 {
                continue;
            }
            if !level.price.is_finite() || level.price <= 0.0 {
                return Err(InvalidLevel::Price(level.price));
            }
            valid.push(level);
        }
        Ok(Levels(valid))
    }

    /// Appends another ladder's levels after this one's.
    pub fn extend(&mut self, other: Levels) {
        self.0.extend(other.0);
    }

    /// Consumes `amount_in` level by level, taking as much as each level offers, until the input
    /// is spent or the ladder is exhausted. Never fails: an amount the ladder cannot absorb comes
    /// back as `remaining_in`.
    pub fn fill(&self, amount_in: f64) -> Fill {
        let mut remaining_in = amount_in;
        let mut amount_out = 0.0;
        for level in &self.0 {
            if remaining_in <= 0.0 {
                break;
            }
            let taken = remaining_in.min(level.quantity);
            amount_out += taken * level.price;
            remaining_in -= taken;
        }
        Fill { amount_out, remaining_in }
    }

    /// Re-expresses the ladder for the opposite trade direction: `(quote per base, base
    /// quantity)` becomes `(base per quote, quote quantity)`. A level whose inverse overflows to
    /// a non-finite value is dropped.
    pub fn invert(&self) -> Levels {
        Levels(
            self.0
                .iter()
                .map(|level| PriceLevel {
                    price: 1.0 / level.price,
                    quantity: level.quantity * level.price,
                })
                .filter(PriceLevel::is_valid)
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
    /// absorbs when it cannot absorb everything. `None` when nothing is consumed: an empty
    /// ladder or a non-positive amount.
    pub fn average_price(&self, amount_in: f64) -> Option<f64> {
        if self.0.is_empty() || amount_in <= 0.0 {
            return None;
        }
        let fill = self.fill(amount_in);
        let consumed = amount_in - fill.remaining_in;
        (consumed > 0.0).then(|| fill.amount_out / consumed)
    }
}

impl Deref for Levels {
    type Target = [PriceLevel];

    fn deref(&self) -> &[PriceLevel] {
        &self.0
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
