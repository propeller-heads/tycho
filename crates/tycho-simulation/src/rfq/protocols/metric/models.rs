use std::str::FromStr;

use alloy::primitives::Address;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tycho_common::Bytes;

use crate::rfq::errors::RFQError;

const Q64_FLOAT: f64 = 18_446_744_073_709_551_616.0;

/// The `PaginatedMetadataResponse` envelope returned by `GET /public/v1/evm/{chain_id}/metadata`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PaginatedMetadataResponse {
    pub data: Vec<MetricMetadata>,
    /// `offset` for the next page, or `None` on the last page.
    #[serde(rename = "nextOffset", default)]
    pub next_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricMetadata {
    #[serde(rename = "poolAddress", deserialize_with = "deserialize_address")]
    pub pool_address: Bytes,
    #[serde(deserialize_with = "deserialize_address")]
    pub token0: Bytes,
    #[serde(deserialize_with = "deserialize_address")]
    pub token1: Bytes,
    /// Total value locked in the requested fiat currency. Absent when Metric has no price for the
    /// pool; used directly as the component TVL. Not carried through the component attributes, so
    /// it is `None` once a state is reconstructed by the decoder.
    #[serde(rename = "tvlFiat", default)]
    pub tvl_fiat: Option<f64>,
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
    #[serde(rename = "bidAdj")]
    pub bid_adj: String,
    #[serde(rename = "askAdj")]
    pub ask_adj: String,
    /// Token0 available for the quote (raw units). `None` when the pool cannot currently quote.
    #[serde(rename = "totalToken0Available", default)]
    pub total_token0_available: Option<String>,
    /// Token1 available for the quote (raw units). `None` when the pool cannot currently quote.
    #[serde(rename = "totalToken1Available", default)]
    pub total_token1_available: Option<String>,
    /// Server Unix timestamp (seconds) when the quote was produced.
    #[serde(rename = "serverTs")]
    pub server_ts: u64,
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
    /// Cumulative output-token volume from the current position to this boundary (raw units).
    #[serde(rename = "cumulativeVolume")]
    pub cumulative_volume: String,
    /// Cumulative input-token amount required to reach this boundary (raw units). Used to drive
    /// the input-based depth walk so pricing matches Metric's own accounting.
    #[serde(rename = "cumulativeInputVolume")]
    pub cumulative_input_volume: String,
}

impl MetricBidAskResponse {
    pub fn bid_price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.bid_adj)
    }

    pub fn ask_price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.ask_adj)
    }

    pub fn total_token0_available(&self) -> Result<BigUint, RFQError> {
        parse_optional_biguint(&self.total_token0_available, "totalToken0Available")
    }

    pub fn total_token1_available(&self) -> Result<BigUint, RFQError> {
        parse_optional_biguint(&self.total_token1_available, "totalToken1Available")
    }

    /// Whether the pool can currently be quoted. v1 has no `quoteAvailable` flag, so availability
    /// is inferred from parseable bid/ask prices and both non-null availability figures.
    pub fn is_quotable(&self) -> bool {
        self.bid_price().is_ok() &&
            self.ask_price().is_ok() &&
            self.total_token0_available().is_ok() &&
            self.total_token1_available().is_ok()
    }
}

impl MetricDepthBin {
    pub fn price(&self) -> Result<f64, RFQError> {
        q64_decimal_to_f64(&self.price)
    }

    pub fn cumulative_volume(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.cumulative_volume, "depth.cumulativeVolume")
    }

    pub fn cumulative_input_volume(&self) -> Result<BigUint, RFQError> {
        parse_biguint(&self.cumulative_input_volume, "depth.cumulativeInputVolume")
    }
}

// Metric's APIs return Q64 values as decimal strings. Convert only when pricing.
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

fn parse_optional_biguint(value: &Option<String>, field: &str) -> Result<BigUint, RFQError> {
    let value = value
        .as_deref()
        .ok_or_else(|| RFQError::ParsingError(format!("{field} is null")))?;
    parse_biguint(value, field)
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
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "totalToken0Available": "1000000000000000000",
            "totalToken1Available": "3000000000",
            "serverTs": 0,
            "depth": {
                "asks": [{
                    "binIdx": 0,
                    "price": "57184906628499610009600",
                    "cumulativeVolume": "1000000000000000000",
                    "cumulativeInputVolume": "3100000000",
                    "priceImpactE6": "33333"
                }],
                "bids": [{
                    "binIdx": -1,
                    "price": "53495557813757699686400",
                    "cumulativeVolume": "3000000000",
                    "cumulativeInputVolume": "1000000000000000000",
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
        assert_eq!(
            response.depth.bids[0]
                .cumulative_input_volume()
                .unwrap(),
            BigUint::from(1_000_000_000_000_000_000u64)
        );
    }

    #[test]
    fn test_bid_ask_null_totals_are_not_quotable() {
        let response: MetricBidAskResponse = serde_json::from_value(serde_json::json!({
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "totalToken0Available": null,
            "totalToken1Available": null,
            "serverTs": 1_770_053_095u64,
        }))
        .unwrap();

        assert_eq!(response.total_token0_available, None);
        assert!(!response.is_quotable());
    }

    #[test]
    fn test_paginated_metadata_deserializes_tvl_fiat() {
        let response: PaginatedMetadataResponse = serde_json::from_value(serde_json::json!({
            "data": [{
                "pair": "ethusdc",
                "poolAddress": "0xbF48bCf474d57fF82A3215319229e0DE1476A557",
                "token0": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "token1": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "tvlFiat": 1234.56
            }],
            "total": 1,
            "nextOffset": null
        }))
        .unwrap();

        assert_eq!(response.next_offset, None);
        assert_eq!(response.data[0].tvl_fiat, Some(1234.56));
    }
}
