use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tycho_client::feed::{BlockHeader, HeaderLike};
use tycho_common::Bytes;

#[derive(Clone, Default, Debug)]
pub struct TimestampHeader {
    pub timestamp: u64,
}

impl HeaderLike for TimestampHeader {
    fn block(self) -> Option<BlockHeader> {
        None
    }

    fn block_number_or_timestamp(self) -> u64 {
        self.timestamp
    }
}

/// One level of a market maker's book: `quantity` base tokens at `price` quote tokens each,
/// both in whole units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    #[serde(
        rename = "q",
        deserialize_with = "deserialize_string_to_f64",
        serialize_with = "serialize_f64_to_string"
    )]
    pub quantity: f64,
    #[serde(
        rename = "p",
        deserialize_with = "deserialize_string_to_f64",
        serialize_with = "serialize_f64_to_string"
    )]
    pub price: f64,
}

fn deserialize_string_to_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse()
        .map_err(serde::de::Error::custom)
}

fn serialize_f64_to_string<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Consumes `levels` in order until `amount_in` is filled or they run out. Returns the amount
/// out and the amount in that no level filled.
pub fn fill_levels(levels: &[PriceLevel], amount_in: f64) -> (f64, f64) {
    let mut remaining_amount_in = amount_in;
    let mut amount_out = 0.0;
    for level in levels {
        if remaining_amount_in <= 0.0 {
            break;
        }
        let filled = remaining_amount_in.min(level.quantity);
        amount_out += filled * level.price;
        remaining_amount_in -= filled;
    }
    (amount_out, remaining_amount_in)
}

/// How often one route may take quotes from an RFQ venue.
///
/// A swap records what it used in the state it returns, so the next swap on that state sees it.
/// The rule travels with the venue's component as the `quote_rule` static attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteRule {
    /// Every market maker quotes once per route. Only for a venue that names its makers and lets
    /// the taker pick one.
    OncePerMaker,
    /// The venue quotes once per route.
    OncePerVenue,
}

impl QuoteRule {
    pub const ATTRIBUTE: &'static str = "quote_rule";
    const ALL: [QuoteRule; 2] = [QuoteRule::OncePerMaker, QuoteRule::OncePerVenue];

    /// The rule as its static attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            QuoteRule::OncePerMaker => "once_per_maker",
            QuoteRule::OncePerVenue => "once_per_venue",
        }
    }

    /// The rule a component's static attributes carry, if any.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Option<QuoteRule>, String> {
        let Some(value) = attributes.get(Self::ATTRIBUTE) else {
            return Ok(None);
        };
        Self::ALL
            .into_iter()
            .find(|rule| rule.as_str().as_bytes() == value.as_ref())
            .map(Some)
            .ok_or_else(|| {
                format!("Unknown quote_rule attribute: {}", String::from_utf8_lossy(value))
            })
    }

    /// Whether `market_maker` may still quote after the makers in `used` did.
    pub fn allows(self, used: &HashSet<String>, market_maker: &str) -> bool {
        match self {
            QuoteRule::OncePerMaker => !used.contains(market_maker),
            QuoteRule::OncePerVenue => used.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::once_per_maker(QuoteRule::OncePerMaker)]
    #[case::once_per_venue(QuoteRule::OncePerVenue)]
    fn quote_rule_attribute_round_trip(#[case] rule: QuoteRule) {
        let attributes =
            HashMap::from([(QuoteRule::ATTRIBUTE.to_string(), rule.as_str().as_bytes().into())]);
        assert_eq!(QuoteRule::from_attributes(&attributes), Ok(Some(rule)));
        assert_eq!(serde_json::to_string(&rule).unwrap(), format!("\"{}\"", rule.as_str()));
    }

    #[test]
    fn quote_rule_absent_attribute() {
        assert_eq!(QuoteRule::from_attributes(&HashMap::new()), Ok(None));
    }

    #[test]
    fn quote_rule_unknown_attribute() {
        let attributes = HashMap::from([(QuoteRule::ATTRIBUTE.to_string(), b"twice".into())]);
        assert!(QuoteRule::from_attributes(&attributes).is_err());
    }

    #[rstest]
    #[case::maker_unused(QuoteRule::OncePerMaker, &["other"], true)]
    #[case::maker_used(QuoteRule::OncePerMaker, &["mm"], false)]
    #[case::venue_unused(QuoteRule::OncePerVenue, &[], true)]
    #[case::venue_used(QuoteRule::OncePerVenue, &["other"], false)]
    fn quote_rule_allows(#[case] rule: QuoteRule, #[case] used: &[&str], #[case] allowed: bool) {
        let used = used
            .iter()
            .map(|maker| maker.to_string())
            .collect();
        assert_eq!(rule.allows(&used, "mm"), allowed);
    }

    #[test]
    fn fill_levels_stops_when_levels_run_out() {
        let levels = vec![
            PriceLevel { quantity: 1.0, price: 3000.0 },
            PriceLevel { quantity: 2.0, price: 2999.0 },
        ];
        assert_eq!(fill_levels(&levels, 1.0), (3000.0, 0.0));
        assert_eq!(fill_levels(&levels, 2.0), (5999.0, 0.0));
        assert_eq!(fill_levels(&levels, 5.0), (8998.0, 2.0));
    }
}
