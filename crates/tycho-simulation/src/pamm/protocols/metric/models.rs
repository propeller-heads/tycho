use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DefaultOnNull, DisplayFromStr};
use tycho_common::Bytes;

use crate::{serde_helpers::evm_address, snapshot_feed::errors::FeedError};

const Q64_FLOAT: f64 = 18_446_744_073_709_551_616.0;

/// The `PaginatedMetadataResponse` envelope returned by `GET /public/v1/evm/{chain_id}/metadata`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginatedMetadataResponse {
    pub data: Vec<MetricMetadata>,
    /// `offset` for the next page, or `None` on the last page.
    pub next_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricMetadata {
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub pool_address: Bytes,
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub token0: Bytes,
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub token1: Bytes,
    /// Total value locked in the requested fiat currency. Absent when Metric has no price for the
    /// pool; used directly as the component TVL. Not carried through the component attributes, so
    /// it is `None` once a state is reconstructed by the decoder.
    pub tvl_fiat: Option<f64>,
}

/// Metric returns its numeric fields as decimal strings; they are parsed once at deserialization
/// (`DisplayFromStr`) so the pricing paths never re-parse, and serialize back to the same form.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricBidAskResponse {
    /// Fee-adjusted bid price, Q64.64.
    #[serde_as(as = "DisplayFromStr")]
    pub bid_adj: BigUint,
    /// Fee-adjusted ask price, Q64.64.
    #[serde_as(as = "DisplayFromStr")]
    pub ask_adj: BigUint,
    /// Token0 available for the quote (raw units). `None` when the pool cannot currently quote.
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub total_token0_available: Option<BigUint>,
    /// Token1 available for the quote (raw units). `None` when the pool cannot currently quote.
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub total_token1_available: Option<BigUint>,
    /// Server Unix timestamp (seconds) when the quote was produced.
    pub server_ts: u64,
    /// Price-provider health for this quote: `healthy`, `feed_down` (no valid price right now),
    /// or `internal_error`. `None` when the field is absent (older responses, or states rebuilt
    /// from component attributes).
    pub price_provider_status: Option<String>,
    /// Per-side depth bins. Absent on older responses and explicitly `null` when the endpoint is
    /// queried with `depth=false`; both decode to an empty book.
    #[serde_as(as = "DefaultOnNull")]
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

#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricDepthBin {
    pub bin_idx: i64,
    /// Fee-adjusted price at this bin boundary, Q64.64.
    #[serde_as(as = "DisplayFromStr")]
    pub price: BigUint,
    /// Cumulative output-token volume from the current position to this boundary (raw units).
    #[serde_as(as = "DisplayFromStr")]
    pub cumulative_volume: BigUint,
    /// Cumulative input-token amount required to reach this boundary (raw units). Used to drive
    /// the input-based depth walk so pricing matches Metric's own accounting.
    #[serde_as(as = "DisplayFromStr")]
    pub cumulative_input_volume: BigUint,
}

impl MetricBidAskResponse {
    pub fn bid_price(&self) -> Result<f64, FeedError> {
        q64_to_f64(&self.bid_adj)
    }

    pub fn ask_price(&self) -> Result<f64, FeedError> {
        q64_to_f64(&self.ask_adj)
    }

    pub fn total_token0_available(&self) -> Result<BigUint, FeedError> {
        self.total_token0_available
            .clone()
            .ok_or_else(|| FeedError::Parsing("totalToken0Available is null".to_string()))
    }

    pub fn total_token1_available(&self) -> Result<BigUint, FeedError> {
        self.total_token1_available
            .clone()
            .ok_or_else(|| FeedError::Parsing("totalToken1Available is null".to_string()))
    }

    /// Whether the pool can currently be quoted.
    ///
    /// The API's own `priceProviderStatus` is the primary gate: anything other than `healthy`
    /// (e.g. `feed_down`, `internal_error`) is not quotable. When the field is absent the
    /// structural checks below decide alone; they are always applied as defence in depth:
    /// both availability figures must be non-null and the order book non-empty. Pools with empty
    /// depth (e.g. `bidAdj=0` / `askAdj` sentinel and no bins) are treated as not quotable, since
    /// depth-based pricing has no bins to walk.
    pub fn is_quotable(&self) -> bool {
        if let Some(status) = self.price_provider_status.as_deref() {
            if status != "healthy" {
                return false;
            }
        }
        self.total_token0_available.is_some() &&
            self.total_token1_available.is_some() &&
            !(self.depth.bids.is_empty() && self.depth.asks.is_empty())
    }
}

/// Converts a Q64.64 fixed-point value to an f64 price.
fn q64_to_f64(value: &BigUint) -> Result<f64, FeedError> {
    let raw = value
        .to_f64()
        .ok_or_else(|| FeedError::Parsing(format!("Q64 price does not fit in f64: {value}")))?;
    Ok(raw / Q64_FLOAT)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(q64_to_f64(&response.depth.asks[0].price).unwrap(), 3100.0);
        assert_eq!(response.depth.bids[0].cumulative_volume, BigUint::from(3_000_000_000u64));
        assert_eq!(
            response.depth.bids[0].cumulative_input_volume,
            BigUint::from(1_000_000_000_000_000_000u64)
        );

        // Numeric fields round-trip back to the same decimal-string JSON form.
        let serialized = serde_json::to_value(&response).unwrap();
        assert_eq!(serialized["bidAdj"], "55340232221128654848000");
        assert_eq!(serialized["totalToken1Available"], "3000000000");
        assert_eq!(serialized["depth"]["asks"][0]["cumulativeInputVolume"], "3100000000");
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
    fn test_bid_ask_absent_totals_decode_as_none() {
        let response: MetricBidAskResponse = serde_json::from_value(serde_json::json!({
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "serverTs": 1_770_053_095u64,
        }))
        .unwrap();

        assert_eq!(response.total_token0_available, None);
        assert_eq!(response.total_token1_available, None);
        assert_eq!(response.price_provider_status, None);
    }

    #[test]
    fn test_bid_ask_null_depth_decodes_as_empty() {
        // `depth=false` makes the endpoint return an explicit null rather than omitting the key.
        let response: MetricBidAskResponse = serde_json::from_value(serde_json::json!({
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "totalToken0Available": "1000000000000000000",
            "totalToken1Available": "3000000000",
            "serverTs": 0,
            "depth": null,
        }))
        .unwrap();

        assert_eq!(response.depth, MetricDepth::default());
        assert!(!response.is_quotable());
    }

    #[test]
    fn test_bid_ask_empty_depth_is_not_quotable() {
        // Real degenerate pool observed on Base: bidAdj=0, askAdj=uint128 max sentinel, no depth.
        let response: MetricBidAskResponse = serde_json::from_value(serde_json::json!({
            "bidAdj": "0",
            "askAdj": "340282366920938463463374607431768211455",
            "totalToken0Available": "11419581536531814910",
            "totalToken1Available": "12935676138",
            "serverTs": 1_784_611_959u64,
        }))
        .unwrap();

        assert!(response.depth.bids.is_empty() && response.depth.asks.is_empty());
        assert!(!response.is_quotable());
    }

    #[test]
    fn test_bid_ask_unhealthy_provider_status_is_not_quotable() {
        // Structurally complete quote, but the API reports the price provider as down: the
        // explicit status must veto quotability regardless of the structural checks.
        let quotable = serde_json::json!({
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "totalToken0Available": "1000000000000000000",
            "totalToken1Available": "3000000000",
            "serverTs": 1_787_710_476u64,
            "depth": {
                "asks": [{
                    "binIdx": 0,
                    "price": "57184906628499610009600",
                    "cumulativeVolume": "1000000000000000000",
                    "cumulativeInputVolume": "3100000000"
                }],
                "bids": []
            }
        });

        for (status, expected) in
            [("healthy", true), ("feed_down", false), ("internal_error", false)]
        {
            let mut payload = quotable.clone();
            payload["priceProviderStatus"] = serde_json::json!(status);
            let response: MetricBidAskResponse = serde_json::from_value(payload).unwrap();
            assert_eq!(response.is_quotable(), expected, "status {status}");
        }

        // Absent status (older responses, attribute round-trips): structural checks decide.
        let response: MetricBidAskResponse = serde_json::from_value(quotable).unwrap();
        assert_eq!(response.price_provider_status, None);
        assert!(response.is_quotable());
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
