use std::collections::HashMap;

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

/// How often one route may take quotes from an RFQ venue.
///
/// A swap records what it used in the state it returns, so the next swap on that state sees it.
/// The rule travels with the venue's component as the `quote_rule` static attribute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteRule {
    /// Every market maker quotes once per route. Only for a venue that names its makers and lets
    /// the taker pick one.
    #[default]
    OncePerMaker,
    /// The venue quotes once per route.
    OncePerVenue,
    /// No limit.
    None,
}

impl QuoteRule {
    pub const ATTRIBUTE: &'static str = "quote_rule";

    /// The rule as its static attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            QuoteRule::OncePerMaker => "once_per_maker",
            QuoteRule::OncePerVenue => "once_per_venue",
            QuoteRule::None => "none",
        }
    }

    /// The rule a component's static attributes carry, or `default` when they carry none.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
        default: QuoteRule,
    ) -> Result<QuoteRule, String> {
        let Some(value) = attributes.get(Self::ATTRIBUTE) else {
            return Ok(default);
        };
        match value.as_ref() {
            b"once_per_maker" => Ok(QuoteRule::OncePerMaker),
            b"once_per_venue" => Ok(QuoteRule::OncePerVenue),
            b"none" => Ok(QuoteRule::None),
            other => {
                Err(format!("Unknown quote_rule attribute: {}", String::from_utf8_lossy(other)))
            }
        }
    }
}
