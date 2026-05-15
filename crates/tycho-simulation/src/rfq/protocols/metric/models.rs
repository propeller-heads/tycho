use std::str::FromStr;

use alloy::primitives::Address;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::rfq::errors::RFQError;

const Q64_FLOAT: f64 = 18_446_744_073_709_551_616.0;

pub const ORACLE_UPDATE_POLICY_ATTR: &str = "oracle_update_policy";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MetricOracleUpdatePolicy {
    #[default]
    Never = 0,
    Always = 1,
    RetryOnRevert = 2,
}

impl MetricOracleUpdatePolicy {
    pub fn default_for_chain(chain: Chain) -> Self {
        match chain {
            Chain::Ethereum => Self::Always,
            _ => Self::Never,
        }
    }

    pub fn as_attribute_value(self) -> Bytes {
        vec![self as u8].into()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricMetadata {
    pub pair: String,
    #[serde(rename = "poolAddress", deserialize_with = "deserialize_address")]
    pub pool_address: Bytes,
    #[serde(
        rename = "priceProvider_address",
        alias = "priceProviderAddress",
        deserialize_with = "deserialize_address"
    )]
    pub price_provider_address: Bytes,
    #[serde(
        rename = "quoterAddress",
        alias = "routerAddress",
        deserialize_with = "deserialize_address"
    )]
    pub quoter_address: Bytes,
    #[serde(deserialize_with = "deserialize_address")]
    pub token0: Bytes,
    #[serde(deserialize_with = "deserialize_address")]
    pub token1: Bytes,
    #[serde(rename = "cexStep")]
    pub cex_step: Option<f64>,
    #[serde(rename = "dexStep")]
    pub dex_step: Option<f64>,
}

fn deserialize_address<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let address = Address::from_str(&s).map_err(serde::de::Error::custom)?;
    Bytes::from_str(&address.to_checksum(None)).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricBidAskResponse {
    pub pair: String,
    #[serde(rename = "bidAdj")]
    pub bid_adj: String,
    #[serde(rename = "askAdj")]
    pub ask_adj: String,
    #[serde(rename = "quoteAvailable")]
    pub quote_available: bool,
    #[serde(rename = "totalToken0Available")]
    pub total_token0_available: String,
    #[serde(rename = "totalToken1Available")]
    pub total_token1_available: String,
    #[serde(rename = "latestBlock")]
    pub latest_block: u64,
    #[serde(rename = "blockTs")]
    pub block_ts: u64,
    #[serde(rename = "serverTs")]
    pub server_ts: u64,
    #[serde(rename = "quoteExpiration")]
    pub quote_expiration: u64,
    #[serde(default)]
    pub depth: MetricDepth,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricDepth {
    #[serde(default)]
    pub asks: Vec<MetricDepthBin>,
    #[serde(default)]
    pub bids: Vec<MetricDepthBin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDepthBin {
    #[serde(rename = "binIdx")]
    pub bin_idx: i64,
    pub price: String,
    #[serde(rename = "cumulativeVolume")]
    pub cumulative_volume: String,
    #[serde(rename = "priceImpactE6")]
    pub price_impact_e6: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSignedOracleUpdateResponse {
    #[serde(rename = "blockTs")]
    pub block_ts: u64,
    #[serde(rename = "serverTs")]
    pub server_ts: u64,
    #[serde(rename = "feedCreator", deserialize_with = "deserialize_address")]
    pub feed_creator: Bytes,
    #[serde(default)]
    pub slots: Vec<MetricSignedOracleUpdateSlot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSignedOracleUpdateSlot {
    #[serde(rename = "slotId")]
    pub slot_id: u64,
    pub deadline: u64,
    #[serde(rename = "slotPairs", default)]
    pub slot_pairs: Vec<String>,
    #[serde(rename = "newSlotValue")]
    pub new_slot_value: String,
    pub signature: Bytes,
    #[serde(default)]
    pub prices: serde_json::Value,
}

impl MetricBidAskResponse {
    pub fn bid_price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.bid_adj)
    }

    pub fn ask_price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.ask_adj)
    }

    pub fn total_token0_available(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.total_token0_available, "totalToken0Available")
    }

    pub fn total_token1_available(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.total_token1_available, "totalToken1Available")
    }
}

impl MetricDepthBin {
    pub fn price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.price)
    }

    pub fn cumulative_volume(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.cumulative_volume, "depth.cumulativeVolume")
    }
}

// Metric's APIs return Q64 values as decimal strings. We only convert them for indicative
// routing; the binding quote path keeps the original Q64 strings for calldata encoding.
pub fn q64_decimal_to_f64(value: &str) -> Result<f64, RFQError> {
    let raw = parse_biguint(value, "Q64 price")?;
    let raw = raw
        .to_f64()
        .ok_or_else(|| RFQError::ParsingError(format!("Q64 price does not fit in f64: {value}")))?;
    Ok(raw / Q64_FLOAT)
}

fn parse_biguint(value: &str, field: &str) -> Result<BigUint, RFQError> {
    BigUint::from_str(value)
        .map_err(|_| RFQError::ParsingError(format!("Failed to parse {field}: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_q64_decimal_to_f64() {
        let one = "18446744073709551616";
        assert_eq!(q64_decimal_to_f64(one).unwrap(), 1.0);
    }

    #[test]
    fn test_bid_ask_deserializes_depth_bins() {
        let response: MetricBidAskResponse = serde_json::from_value(serde_json::json!({
            "pair": "wethusdc",
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "quoteAvailable": true,
            "totalToken0Available": "1000000000000000000",
            "totalToken1Available": "3000000000",
            "latestBlock": 0,
            "blockTs": 0,
            "serverTs": 1,
            "quoteExpiration": 0,
            "depth": {
                "asks": [{
                    "binIdx": 0,
                    "price": "57184906628499610009600",
                    "cumulativeVolume": "1000000000000000000",
                    "priceImpactE6": "33333"
                }],
                "bids": [{
                    "binIdx": -1,
                    "price": "53495557813757699686400",
                    "cumulativeVolume": "3000000000",
                    "priceImpactE6": "33333"
                }]
            }
        }))
        .unwrap();

        assert_eq!(response.depth.asks.len(), 1);
        assert_eq!(response.depth.bids[0].bin_idx, -1);
        assert_eq!(response.depth.asks[0].price().unwrap(), 3100.0);
        assert_eq!(
            response.depth.bids[0]
                .cumulative_volume()
                .unwrap(),
            BigUint::from(3_000_000_000u64)
        );
    }
}
