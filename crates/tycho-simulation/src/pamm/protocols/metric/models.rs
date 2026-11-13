use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DefaultOnNull, DisplayFromStr};
use tycho_common::Bytes;

use crate::{serde_helpers::evm_address, snapshot_feed::errors::FeedError};

const Q64_FLOAT: f64 = 18_446_744_073_709_551_616.0;

/// The one `priceProviderStatus` Metric allows trading on.
const HEALTHY_STATUS: &str = "healthy";

/// The body Metric answers a request it will not serve with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MetricApiError {
    pub error: String,
}

impl MetricApiError {
    /// Whether this is Metric stating it does not serve the chain its path named. A pool it
    /// does not know answers with the same status, so the chain is named in the message.
    pub fn refuses_the_chain(&self) -> bool {
        self.error
            .starts_with("Unknown chainId")
    }
}

/// The `PaginatedMetadataResponse` envelope returned by `GET /public/v1/evm/{chain_id}/metadata`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginatedMetadataResponse {
    pub data: Vec<MetricMetadata>,
    /// `offset` for the next page, or `None` on the last page.
    pub next_offset: Option<u64>,
}

/// One Metric pool's address, tokens, USD TVL and pause level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricMetadata {
    /// The pool contract's address.
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub pool_address: Bytes,
    /// The pool's first token; bids and asks are quoted for it.
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub token0: Bytes,
    /// The pool's second token, in which token0 is priced.
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub token1: Bytes,
    /// Total value locked in the requested fiat currency, used directly as the component TVL.
    /// Absent when Metric has no price for the pool.
    pub tvl_fiat: Option<f64>,
    /// `0` trades normally; anything higher refuses some or all swaps. `None` when the response
    /// omits the field.
    #[serde(default)]
    pub pause_level: Option<u8>,
}

impl MetricMetadata {
    /// Whether the pool accepts swaps. Metric routes only to pools at pause level `0`; a pool
    /// whose metadata carries no pause level is treated as unpaused.
    pub fn is_unpaused(&self) -> bool {
        self.pause_level.unwrap_or(0) == 0
    }
}

/// The body `POST /public/v1/evm/{chain_id}/bid_ask/batch` is sent with.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricBatchBidAskRequest {
    /// Checksummed pool addresses, at most 100.
    pub pools: Vec<String>,
}

/// The answer of `POST /public/v1/evm/{chain_id}/bid_ask/batch`: one result per requested pool.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MetricBatchBidAskResponse {
    pub results: Vec<MetricBatchBidAskResult>,
}

/// One pool's entry in a batch bid/ask answer: its quote, or the error the single-pool endpoint
/// would have answered for it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricBatchBidAskResult {
    #[serde(deserialize_with = "evm_address::deserialize")]
    pub pool_address: Bytes,
    #[serde(default)]
    pub quote: Option<MetricBidAskResponse>,
    #[serde(default)]
    pub error: Option<MetricBatchBidAskError>,
}

impl MetricBatchBidAskResult {
    /// The pool's quote, or why Metric did not quote it.
    pub fn into_quote(self) -> Result<MetricBidAskResponse, FeedError> {
        match (self.quote, self.error) {
            (Some(quote), _) => Ok(quote),
            (None, Some(error)) => Err(error.into()),
            (None, None) => Err(FeedError::Parsing(format!(
                "Metric batch bid/ask answered neither a quote nor an error for pool {}",
                self.pool_address
            ))),
        }
    }
}

/// A pool a batch bid/ask request could not quote.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MetricBatchBidAskError {
    /// The HTTP status the single-pool endpoint would have answered with.
    pub status: u16,
    pub error: String,
}

impl From<MetricBatchBidAskError> for FeedError {
    /// Classified as a failed single-pool request would be: fatal only when Metric names the
    /// chain as one it does not serve.
    fn from(error: MetricBatchBidAskError) -> Self {
        let message = format!("Metric bid/ask HTTP error {}: {}", error.status, error.error);
        if (MetricApiError { error: error.error }).refuses_the_chain() {
            FeedError::Fatal(message)
        } else {
            FeedError::Connection(message)
        }
    }
}

/// One pool's quote from Metric's bid/ask endpoints.
///
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
    /// Price-provider health for this quote: `healthy`, `feed_down`, `pool_refused`,
    /// `pool_paused`, `internal_error` or `unknown`. `None` when the response omits the field.
    pub price_provider_status: Option<String>,
    /// Per-side depth bins. Absent on older responses and explicitly `null` when the endpoint is
    /// queried with `depth=false`; both decode to an empty book.
    #[serde_as(as = "DefaultOnNull")]
    #[serde(default)]
    pub depth: MetricDepth,
}

/// The depth of one pool's book: the bins a swap walks, per side.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricDepth {
    /// Bins walked when buying token0 with token1.
    #[serde(default)]
    pub asks: Vec<MetricDepthBin>,
    /// Bins walked when selling token0 for token1.
    #[serde(default)]
    pub bids: Vec<MetricDepthBin>,
}

/// One boundary of a side's depth: the price there and what a swap has consumed and received by
/// reaching it.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricDepthBin {
    /// The bin's index on Metric's price grid.
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
    /// Whether the pool quotes a bid: the API sends `bidAdj = 0` when it does not.
    fn quotes_bid(&self) -> bool {
        !self.bid_adj.is_zero()
    }

    /// Whether the pool quotes an ask: the API sends `askAdj = 2^128 - 1` when it does not.
    fn quotes_ask(&self) -> bool {
        self.ask_adj != BigUint::from(u128::MAX)
    }

    /// The bid price, or `None` when the pool quotes no bid.
    pub fn bid_price(&self) -> Result<Option<f64>, FeedError> {
        if !self.quotes_bid() {
            return Ok(None);
        }
        q64_to_f64(&self.bid_adj).map(Some)
    }

    /// The ask price, or `None` when the pool quotes no ask.
    pub fn ask_price(&self) -> Result<Option<f64>, FeedError> {
        if !self.quotes_ask() {
            return Ok(None);
        }
        q64_to_f64(&self.ask_adj).map(Some)
    }

    /// The bins a swap selling token0 walks, when that side can be traded: the pool quotes a bid
    /// and the bid side has depth, meaning its deepest bin reports some volume. Metric closes one
    /// side of a `healthy` pool by sending that side's sentinel price with no bins, and sends a
    /// side with no inventory as bins whose volumes are all zero; both read as untradable.
    pub fn tradable_bids(&self) -> Option<&[MetricDepthBin]> {
        (self.quotes_bid() && has_volume(&self.depth.bids)).then_some(self.depth.bids.as_slice())
    }

    /// The bins a swap buying token0 walks, when that side can be traded: the pool quotes an ask
    /// and the ask side has depth. See [`tradable_bids`](Self::tradable_bids).
    pub fn tradable_asks(&self) -> Option<&[MetricDepthBin]> {
        (self.quotes_ask() && has_volume(&self.depth.asks)).then_some(self.depth.asks.as_slice())
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

    /// Whether the pool can currently be quoted: Metric reports its price provider `healthy`,
    /// both availability figures are present and at least one side is tradable.
    ///
    /// Metric's rule is to trade on nothing but `healthy`, so a quote without a status is not
    /// quotable either. `feed_down`, `pool_refused`, `pool_paused`, `internal_error` and
    /// `unknown` all leave the pool out of this poll's book; the next poll asks again.
    pub fn is_quotable(&self) -> bool {
        self.price_provider_status.as_deref() == Some(HEALTHY_STATUS) &&
            self.total_token0_available.is_some() &&
            self.total_token1_available.is_some() &&
            (self.tradable_bids().is_some() || self.tradable_asks().is_some())
    }
}

/// Whether a side's bins hold anything to trade. Volumes are cumulative, so the deepest bin
/// carries the side's total.
fn has_volume(bins: &[MetricDepthBin]) -> bool {
    bins.last()
        .is_some_and(|deepest| !deepest.cumulative_volume.is_zero())
}

/// Converts a Q64.64 fixed-point value to an f64 price.
fn q64_to_f64(value: &BigUint) -> Result<f64, FeedError> {
    let raw = value
        .to_f64()
        .ok_or_else(|| FeedError::Parsing(format!("Q64 price does not fit in f64: {value}")))?;
    Ok(raw / Q64_FLOAT)
}

/// The pools one Metric poll quoted, in whatever order the venue answered.
///
/// The serde form follows the venue API models, so the writer and the reader must run the same
/// tycho-simulation version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricPrices {
    /// One entry per pool the poll quoted.
    pub pools: Vec<MetricPoolPrice>,
}

/// One pool's metadata and bid/ask quote.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricPoolPrice {
    /// The pool's tokens, TVL and pause level, as of the feed's last refresh of its pool list:
    /// up to 15 s older than the quote, and up to 90 s while refreshes fail. The quote's own
    /// status is current.
    pub metadata: MetricMetadata,
    /// The pool's quote.
    pub bid_ask: MetricBidAskResponse,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;

    #[test]
    fn sentinel_sides_have_no_price() {
        let quote = |bid: String, ask: String| MetricBidAskResponse {
            bid_adj: bid.parse().unwrap(),
            ask_adj: ask.parse().unwrap(),
            total_token0_available: None,
            total_token1_available: None,
            server_ts: 0,
            price_provider_status: None,
            depth: MetricDepth::default(),
        };

        let unquoted = quote("0".to_string(), u128::MAX.to_string());
        assert_eq!(unquoted.bid_price().unwrap(), None);
        assert_eq!(unquoted.ask_price().unwrap(), None);

        // One below the no-ask sentinel is still a price.
        let quoted = quote("1".to_string(), (u128::MAX - 1).to_string());
        assert!(quoted.bid_price().unwrap().is_some());
        assert!(quoted.ask_price().unwrap().is_some());
    }

    #[test]
    fn bid_ask_deserializes_depth_bins() {
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
    fn bid_ask_null_totals_are_not_quotable() {
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
    fn bid_ask_absent_totals_decode_as_none() {
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
    fn bid_ask_null_depth_decodes_as_empty() {
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
    fn bid_ask_empty_depth_is_not_quotable() {
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

    /// A quote with depth on both sides, in the shape the batch endpoint answers.
    fn two_sided_quote() -> serde_json::Value {
        serde_json::json!({
            "bidAdj": "55340232221128654848000",
            "askAdj": "55524699661865750400000",
            "totalToken0Available": "1000000000000000000",
            "totalToken1Available": "3000000000",
            "serverTs": 1_787_710_476u64,
            "priceProviderStatus": "healthy",
            "depth": {
                "asks": [{
                    "binIdx": 0,
                    "price": "57184906628499610009600",
                    "cumulativeVolume": "1000000000000000000",
                    "cumulativeInputVolume": "3100000000"
                }],
                "bids": [{
                    "binIdx": -1,
                    "price": "53495557813757699686400",
                    "cumulativeVolume": "3000000000",
                    "cumulativeInputVolume": "1000000000000000000"
                }]
            }
        })
    }

    /// Metric trades on `healthy` alone, so every other status, and none at all, vetoes an
    /// otherwise complete quote.
    #[rstest]
    #[case::healthy(Some("healthy"), true)]
    #[case::feed_down(Some("feed_down"), false)]
    #[case::pool_refused(Some("pool_refused"), false)]
    #[case::pool_paused(Some("pool_paused"), false)]
    #[case::internal_error(Some("internal_error"), false)]
    #[case::unknown(Some("unknown"), false)]
    #[case::absent(None, false)]
    fn only_a_healthy_provider_is_quotable(#[case] status: Option<&str>, #[case] expected: bool) {
        let mut payload = two_sided_quote();
        match status {
            Some(status) => payload["priceProviderStatus"] = serde_json::json!(status),
            None => {
                payload
                    .as_object_mut()
                    .unwrap()
                    .remove("priceProviderStatus");
            }
        }
        let response: MetricBidAskResponse = serde_json::from_value(payload).unwrap();

        assert_eq!(response.is_quotable(), expected);
    }

    /// A side is tradable only with both a price and depth. Metric closes one side of a
    /// `healthy` pool with that side's sentinel and no bins; the other side keeps trading.
    #[rstest]
    #[case::both_sides(None, None, true, true)]
    #[case::bid_closed(Some(("bidAdj", "0")), Some("bids"), false, true)]
    #[case::ask_closed(
        Some(("askAdj", "340282366920938463463374607431768211455")),
        Some("asks"),
        true,
        false
    )]
    #[case::bid_priced_without_bins(None, Some("bids"), false, true)]
    #[case::ask_bins_without_a_price(
        Some(("askAdj", "340282366920938463463374607431768211455")),
        None,
        true,
        false
    )]
    fn a_side_trades_only_with_a_price_and_depth(
        #[case] sentinel: Option<(&str, &str)>,
        #[case] emptied_side: Option<&str>,
        #[case] bids_tradable: bool,
        #[case] asks_tradable: bool,
    ) {
        let mut payload = two_sided_quote();
        if let Some((field, value)) = sentinel {
            payload[field] = serde_json::json!(value);
        }
        if let Some(side) = emptied_side {
            payload["depth"][side] = serde_json::json!([]);
        }
        let response: MetricBidAskResponse = serde_json::from_value(payload).unwrap();

        assert_eq!(response.tradable_bids().is_some(), bids_tradable);
        assert_eq!(response.tradable_asks().is_some(), asks_tradable);
        assert!(response.is_quotable(), "one tradable side keeps the pool quotable");
    }

    /// Metric sends a side with no inventory as bins whose cumulative volumes are all zero, priced
    /// and under a `healthy` status (LONGSHORT/WETH bids on Ethereum, for one). Such a side fills
    /// nothing, so it is not tradable; the other side keeps the pool quotable.
    #[test]
    fn a_side_whose_bins_hold_no_volume_is_not_tradable() {
        let mut payload = two_sided_quote();
        payload["depth"]["bids"] = serde_json::json!([
            { "binIdx": -1, "price": "53495557813757699686400", "cumulativeVolume": "0",
              "cumulativeInputVolume": "0" },
            { "binIdx": -2, "price": "53311090373020604134400", "cumulativeVolume": "0",
              "cumulativeInputVolume": "0" }
        ]);
        let response: MetricBidAskResponse = serde_json::from_value(payload).unwrap();

        assert!(response.bid_price().unwrap().is_some());
        assert!(response.tradable_bids().is_none());
        assert!(response.tradable_asks().is_some());
        assert!(response.is_quotable());
    }

    /// A pool priced on both sides but with no depth on either has no side to trade.
    #[test]
    fn prices_without_depth_are_not_quotable() {
        let mut payload = two_sided_quote();
        payload["depth"] = serde_json::json!({ "asks": [], "bids": [] });
        let response: MetricBidAskResponse = serde_json::from_value(payload).unwrap();

        assert!(response.bid_price().unwrap().is_some());
        assert!(response.ask_price().unwrap().is_some());
        assert!(!response.is_quotable());
    }

    #[test]
    fn paginated_metadata_deserializes_tvl_fiat() {
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

    fn metadata_with_pause_level(pause_level: Option<u8>) -> MetricMetadata {
        let mut pool = serde_json::json!({
            "poolAddress": "0xbF48bCf474d57fF82A3215319229e0DE1476A557",
            "token0": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            "token1": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        });
        if let Some(level) = pause_level {
            pool["pauseLevel"] = serde_json::json!(level);
        }
        serde_json::from_value(pool).unwrap()
    }

    #[rstest]
    #[case::trading(Some(0), true)]
    #[case::partly_paused(Some(1), false)]
    #[case::fully_paused(Some(2), false)]
    #[case::no_pause_level(None, true)]
    fn only_pause_level_zero_is_unpaused(#[case] pause_level: Option<u8>, #[case] unpaused: bool) {
        let pool = metadata_with_pause_level(pause_level);

        assert_eq!(pool.pause_level, pause_level);
        assert_eq!(pool.is_unpaused(), unpaused);
    }

    #[test]
    fn batch_bid_ask_carries_quotes_and_per_pool_errors() {
        let response: MetricBatchBidAskResponse = serde_json::from_value(serde_json::json!({
            "results": [
                {
                    "poolAddress": "0xf85afbadecc7f23dd173ed706f07d9c7e2473e24",
                    "quote": {
                        "pair": "WETH/USDC",
                        "poolAddress": "0xF85AfbADeCC7F23Dd173ed706f07d9C7e2473e24",
                        "priceProviderStatus": "healthy",
                        "bidAdj": "49940417631045696435290",
                        "askAdj": "49945012360825322506424",
                        "totalToken0Available": "24503618827789131821",
                        "totalToken1Available": "49785157539",
                        "serverTs": 1790508850,
                        "depth": null
                    }
                },
                {
                    "poolAddress": "0x0000000000000000000000000000000000000001",
                    "error": { "status": 404, "error": "Pair metadata not found" }
                },
                { "poolAddress": "0x0000000000000000000000000000000000000002" }
            ]
        }))
        .unwrap();

        let [quoted, failed, empty] = <[_; 3]>::try_from(response.results).unwrap();
        // Addresses decode to bytes, whatever casing the venue answers in.
        assert_eq!(
            quoted.pool_address,
            Bytes::from_str("0xF85AfbADeCC7F23Dd173ed706f07d9C7e2473e24").unwrap()
        );
        assert_eq!(quoted.into_quote().unwrap().server_ts, 1790508850);
        let error = failed.into_quote().unwrap_err();
        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        assert!(
            error
                .to_string()
                .contains("Pair metadata not found"),
            "{error}"
        );
        assert!(matches!(empty.into_quote(), Err(FeedError::Parsing(_))));
    }

    #[rstest]
    #[case::an_unknown_pool("Pair metadata not found", false)]
    #[case::an_unserved_chain("Unknown chainId: 130", true)]
    fn only_a_batch_entry_refusing_the_chain_is_fatal(#[case] message: &str, #[case] fatal: bool) {
        let error: FeedError =
            MetricBatchBidAskError { status: 404, error: message.to_string() }.into();

        assert_eq!(error.is_fatal(), fatal, "{error}");
    }
}
