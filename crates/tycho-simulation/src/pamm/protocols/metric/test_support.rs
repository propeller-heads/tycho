//! Fixtures shared by the tests of the Metric feeds and sources.

use std::{collections::HashMap, str::FromStr, sync::Arc};

use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::{
    book::{test_token_map, BookFeedConfig},
    evm::protocol::utils::bytes_to_address,
    pamm::protocols::metric::{
        client::{MetricClient, DEFAULT_BID_ASK_CONCURRENCY},
        feed::MetricBookAssemblerBuilder,
        models::MetricPrices,
        source::{MetricBookAssembler, MetricPricesSource},
    },
    snapshot_feed::http::test_support::{spawn_http_server, MockHttpServer, MockRequest},
};

pub(super) const FIRST_POOL: &str = "0x1111111111111111111111111111111111111111";
pub(super) const SECOND_POOL: &str = "0x2222222222222222222222222222222222222222";
pub(super) const UNQUOTABLE: &str = "0x3333333333333333333333333333333333333333";
pub(super) const BELOW_FLOOR: &str = "0x4444444444444444444444444444444444444444";
pub(super) const PAUSED: &str = "0x6666666666666666666666666666666666666666";
pub(super) const UNKNOWN_POOL: &str = "0x8888888888888888888888888888888888888888";
pub(super) const SERVER_TS: u64 = 1_700_000_000;

pub(super) fn pool_json(pool: &str, tvl_fiat: Option<f64>) -> serde_json::Value {
    serde_json::json!({
        "poolAddress": pool,
        "token0": weth().address.to_string(),
        "token1": usdc().address.to_string(),
        "tvlFiat": tvl_fiat,
    })
}

pub(super) fn metadata_page(pools: &[serde_json::Value], next_offset: Option<u64>) -> String {
    serde_json::json!({ "data": pools, "nextOffset": next_offset }).to_string()
}

/// The bid/ask body of a healthy, quotable pool; `unhealthy` flips the provider status.
pub(super) fn bid_ask_json(server_ts: u64, unhealthy: bool) -> String {
    let mut body = serde_json::json!({
        "bidAdj": q64(3000).to_string(),
        "askAdj": q64(3010).to_string(),
        "totalToken0Available": "1000000000000000000",
        "totalToken1Available": "3000000000",
        "serverTs": server_ts,
        "depth": {
            "asks": [{
                "binIdx": 0,
                "price": q64(3010).to_string(),
                "cumulativeVolume": "3000000000",
                "cumulativeInputVolume": "1000000000000000000"
            }],
            "bids": []
        }
    });
    if unhealthy {
        body["priceProviderStatus"] = serde_json::json!("degraded");
    }
    body.to_string()
}

/// The batch bid/ask target on Ethereum as the client requests it.
pub(super) const BATCH_TARGET: &str = "/public/v1/evm/1/bid_ask/batch";

/// The pools a batch bid/ask request names, as the lowercase hex its tests address them by.
pub(super) fn requested_pools(request: &MockRequest) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    body["pools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pool| {
            let pool = pool.as_str().unwrap();
            // The client sends checksummed addresses.
            let checksummed = bytes_to_address(&Bytes::from_str(pool).unwrap())
                .unwrap()
                .to_checksum(None);
            assert_eq!(pool, checksummed);
            pool.to_lowercase()
        })
        .collect()
}

/// A batch answer holding `pool`'s quote.
pub(super) fn quote_entry(pool: &str, bid_ask: &str) -> serde_json::Value {
    let quote: serde_json::Value = serde_json::from_str(bid_ask).unwrap();
    serde_json::json!({ "poolAddress": pool, "quote": quote })
}

/// A batch answer holding the error Metric gave for `pool`.
pub(super) fn error_entry(pool: &str, status: u16, error: &str) -> serde_json::Value {
    serde_json::json!({ "poolAddress": pool, "error": { "status": status, "error": error } })
}

pub(super) fn batch_body(entries: Vec<serde_json::Value>) -> String {
    serde_json::json!({ "results": entries }).to_string()
}

pub(super) fn weth() -> Token {
    let address = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
    Token::new(&address, "WETH", 18, 0, &[], Chain::Ethereum, 100)
}

pub(super) fn usdc() -> Token {
    let address = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
    Token::new(&address, "USDC", 6, 0, &[], Chain::Ethereum, 100)
}

pub(super) fn q64(price: u64) -> BigUint {
    BigUint::from(price) << 64usize
}

/// A server holding four pools in the `[WETH, USDC]` universe: a healthy one, one whose quote
/// is unusable, one under the 100 USD floor and a paused one.
pub(super) async fn pool_server() -> MockHttpServer {
    let mut paused = pool_json(PAUSED, Some(1_000.0));
    paused["pauseLevel"] = serde_json::json!(1);
    let page = metadata_page(
        &[
            pool_json(FIRST_POOL, Some(1_234.567_890_123_456_7)),
            pool_json(UNQUOTABLE, Some(1_000.0)),
            pool_json(BELOW_FLOOR, Some(99.0)),
            paused,
        ],
        None,
    );
    spawn_http_server(move |request| {
        if request
            .target
            .starts_with("/public/v1/evm/1/metadata?")
        {
            return Some(("200 OK", page.clone()));
        }
        let entries = requested_pools(request)
            .into_iter()
            .map(|pool| quote_entry(&pool, &bid_ask_json(SERVER_TS, pool == UNQUOTABLE)))
            .collect();
        Some(("200 OK", batch_body(entries)))
    })
    .await
}

/// A prices source against the Metric deployment at `base_url`.
pub(super) fn prices_source(chain: Chain, base_url: &str, api_key: &str) -> MetricPricesSource {
    MetricPricesSource::new(MetricClient::new(
        Some(base_url),
        chain.try_id().unwrap(),
        api_key.to_string(),
        DEFAULT_BID_ASK_CONCURRENCY,
    ))
}

/// An assembler over an empty token universe and no TVL floor.
pub(super) fn assembler_for(chain: Chain) -> MetricBookAssembler {
    MetricBookAssemblerBuilder::new(BookFeedConfig {
        chain,
        tokens: Arc::new(HashMap::new()),
        min_tvl_usd: 0.0,
    })
    .build()
}

/// A book config serving `[WETH, USDC]` above a floor of 100 USD.
pub(super) fn weth_usdc_book_config() -> BookFeedConfig {
    BookFeedConfig {
        chain: Chain::Ethereum,
        tokens: Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ])),
        min_tvl_usd: 100.0,
    }
}

/// An assembler serving `[WETH, USDC]` above a floor of 100 USD.
pub(super) fn serving_weth_usdc_above_100_usd() -> MetricBookAssembler {
    MetricBookAssemblerBuilder::new(weth_usdc_book_config()).build()
}

pub(super) fn pools_of(prices: &MetricPrices) -> Vec<String> {
    let mut pools: Vec<_> = prices
        .pools
        .iter()
        .map(|price| price.metadata.pool_address.to_string())
        .collect();
    pools.sort();
    pools
}
