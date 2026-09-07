use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::bebop::{client::BebopClient, source::BebopBookSource},
    },
    snapshot_feed::{
        errors::FeedError,
        ws::{default_ws_feed_config, run_ws_feed, WsFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Bebop's book feed: streams market-maker price levels over Bebop's pricing WebSocket and
/// publishes every update as a complete [`BookSnapshot`]. Built through [`BebopFeedBuilder`].
#[derive(Clone, Debug)]
pub struct BebopFeed {
    feed_config: WsFeedConfig,
    source: BebopBookSource,
}

/// Builds a [`BebopFeed`] from the shared [`BookFeedConfig`] and Bebop's API key.
///
/// The `origin_*` fields identify the flow behind binding quote requests. Bebop uses them for
/// abuse prevention and address screening, and can configure API accounts to require them,
/// rejecting quote requests in which a required field is missing. See
/// <https://docs.bebop.xyz/rfq-api/guides/best-practices#6-pass-origin-so-we-can-identify-legitimate-flow>.
pub struct BebopFeedBuilder {
    common: BookFeedConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    ws_key: String,
    quote_timeout: Duration,
    origin_address: Option<Bytes>,
    origin_target: Option<Bytes>,
    origin_source: Option<String>,
    feed_config: WsFeedConfig,
}

impl BebopFeedBuilder {
    pub fn new(
        common: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        ws_key: String,
    ) -> Self {
        Self {
            common,
            usd_quote_tokens: usd_quote_tokens.into(),
            ws_key,
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            origin_address: None,
            origin_target: None,
            origin_source: None,
            feed_config: Self::default_feed_config(),
        }
    }

    /// Deadline for binding quote requests. Default: 5 s.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Set the real end-user's EOA, sent as `origin_address` with binding quote requests
    pub fn origin_address(mut self, origin_address: Bytes) -> Self {
        self.origin_address = Some(origin_address);
        self
    }

    /// Set the `to` address of the resulting transaction (e.g. the router contract), sent as
    /// `origin_target` with binding quote requests
    pub fn origin_target(mut self, origin_target: Bytes) -> Self {
        self.origin_target = Some(origin_target);
        self
    }

    /// Set a stable identifier for the upstream flow source, sent as `origin_source` with
    /// binding quote requests
    pub fn origin_source(mut self, origin_source: String) -> Self {
        self.origin_source = Some(origin_source);
        self
    }

    /// The feed tuning a new builder starts from: the shared values every WebSocket feed uses
    /// (see [`WsFeedConfig`]). Spread from it to change individual fields:
    /// `WsFeedConfig { read_idle_timeout: Duration::from_secs(30),
    /// ..BebopFeedBuilder::default_feed_config() }`.
    pub fn default_feed_config() -> WsFeedConfig {
        default_ws_feed_config()
    }

    /// Tune the price feed loop (timeouts, backoff, failure limit)
    pub fn feed_config(mut self, feed_config: WsFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails for chains Bebop does not serve.
    pub fn build(self) -> Result<BebopFeed, FeedError> {
        let url = chain_to_bebop_url(self.common.chain)?;
        Ok(BebopFeed {
            feed_config: self.feed_config,
            source: BebopBookSource {
                common: self.common,
                usd_quote_tokens: self.usd_quote_tokens,
                client: Arc::new(BebopClient::new(
                    format!("https://{url}/quote"),
                    format!("wss://{url}/pricing?format=protobuf"),
                    self.ws_key,
                    self.quote_timeout,
                    self.origin_address,
                    self.origin_target,
                    self.origin_source,
                )),
            },
        })
    }
}

impl SnapshotFeed for BebopFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_ws_feed(self.feed_config, publisher, self.source)
    }
}

/// Maps a Chain to its Bebop API host path (shared by the pricing WebSocket and the quote API)
fn chain_to_bebop_url(chain: Chain) -> Result<String, FeedError> {
    let chain_path = match chain {
        Chain::Ethereum => "ethereum",
        Chain::Base => "base",
        _ => return Err(FeedError::Fatal(format!("Unsupported chain: {chain:?}"))),
    };
    Ok(format!("api.bebop.xyz/pmm/{chain_path}/v3"))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr, time::Duration};

    use dotenv::dotenv;
    use num_bigint::BigUint;
    use tokio::time::timeout;
    use tycho_common::models::protocol::GetAmountOutParams;

    use super::*;
    use crate::{
        book::{test_token_map, Book},
        snapshot_feed::SnapshotFeed,
    };

    /// BebopSettlement.swapSingle
    const SWAP_SINGLE_SELECTOR: [u8; 4] = [0x4d, 0xce, 0xbc, 0xba];
    /// BebopSettlement.swapAggregate
    const SWAP_AGGREGATE_SELECTOR: [u8; 4] = [0xa2, 0xf7, 0x48, 0x93];
    /// BebopRouter.swap
    const ROUTER_SWAP_SELECTOR: [u8; 4] = [0x95, 0x86, 0xd0, 0xe8];

    /// Waits until the receiver holds a snapshot and returns its books.
    async fn next_books(
        rx: &mut tokio::sync::watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
    ) -> Arc<HashMap<String, Book>> {
        loop {
            rx.changed().await.expect("feed ended");
            if let Some(snapshot) = &*rx.borrow_and_update() {
                return Arc::clone(&snapshot.books);
            }
        }
    }

    #[tokio::test]
    #[ignore = "hits Bebop's live API; requires BEBOP_KEY"]
    async fn test_bebop_websocket_connection() {
        // We test with quote tokens that are not USDC in order to ensure our normalization works
        // fine
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        dotenv().expect("Missing .env file");
        let key = std::env::var("BEBOP_KEY").expect("BEBOP_KEY not set");

        let quote_tokens = HashSet::from([
            // Use addresses we forgot to checksum (to test checksumming)
            Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(), // USDC
            Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(), // USDT
        ]);

        let common = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&wbtc, "WBTC", 8)])),
            min_tvl_usd: 10.0,
        };
        let feed = BebopFeedBuilder::new(common, quote_tokens, key)
            .quote_timeout(Duration::from_secs(30))
            .build()
            .unwrap();

        let (publisher, mut rx) = Publisher::channel();
        let _feed = tokio::spawn(feed.run(publisher));

        // Receiving a single decodable book is enough to prove the authenticated handshake and
        // protobuf decoding work. Bebop only pushes on price changes, so requiring more makes
        // the test flaky against market cadence.
        let pairs = timeout(Duration::from_secs(10), next_books(&mut rx))
            .await
            .expect("no book received within 10 seconds");

        assert!(!pairs.is_empty());
        println!("Received {} components in this book", pairs.len());
        for pair in pairs.values() {
            assert_eq!(pair.component.protocol_system, "book:bebop");
            assert_eq!(pair.component.protocol_type_name, "bebop_pool");
            assert_eq!(pair.component.chain, Chain::Ethereum);

            // A streamed state carries price levels: it can quote a spot price.
            let [base, quote] = pair.component.tokens.as_slice() else {
                panic!("expected exactly two tokens")
            };
            assert!(
                pair.state
                    .spot_price(base, quote)
                    .unwrap() >
                    0.0
            );
        }
    }

    #[tokio::test]
    #[ignore = "hits Bebop's live API; requires BEBOP_KEY"]
    async fn test_bebop_quote_single_order() {
        let token_in = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let token_out = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        dotenv().expect("Missing .env file");
        let key = std::env::var("BEBOP_KEY").expect("BEBOP_KEY not set");

        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        let common = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[
                (&token_in, "TOKEN_IN", 18),
                (&token_out, "TOKEN_OUT", 18),
            ])),
            min_tvl_usd: 10.0,
        };
        let feed = BebopFeedBuilder::new(common, HashSet::new(), key)
            .quote_timeout(Duration::from_secs(30))
            .origin_address(Bytes::from_str("0x00000000219ab540356cBB839Cbe05303d7705Fa").unwrap())
            .origin_target(router.clone())
            .origin_source("tycho-test".to_string())
            .build()
            .unwrap();

        let params = GetAmountOutParams {
            amount_in: BigUint::from(1_000000000000000000u64),
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            sender: router.clone(),
            receiver: router,
        };
        let quote = feed
            .source
            .client
            .request_binding_quote(&params)
            .await
            .unwrap();

        assert_eq!(quote.base_token, token_in);
        assert_eq!(quote.quote_token, token_out);
        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));

        // Conservative sanity bound (0.01 WBTC for 1 WETH) — proves a real, non-dust quote came
        // back without depending closely on the live WETH/WBTC price.
        assert!(quote.amount_out > BigUint::from(1_000_000u64));

        // The settlement mode depends on the API account configuration behind BEBOP_KEY:
        // settlement-mode accounts get BebopSettlement.swapSingle calldata, router-mode
        // accounts get BebopRouter.swap calldata.
        let selector = &quote
            .quote_attributes
            .get("calldata")
            .unwrap()[..4];
        if selector == SWAP_SINGLE_SELECTOR {
            let partial_fill_offset_slice = quote
                .quote_attributes
                .get("partial_fill_offset")
                .unwrap()
                .as_ref();
            let mut partial_fill_offset_array = [0u8; 8];
            partial_fill_offset_array.copy_from_slice(partial_fill_offset_slice);

            assert_eq!(u64::from_be_bytes(partial_fill_offset_array), 12);
        } else {
            assert_eq!(selector, ROUTER_SWAP_SELECTOR);
        }
    }

    #[tokio::test]
    #[ignore = "hits Bebop's live API; requires BEBOP_KEY"]
    async fn test_bebop_quote_aggregate_order() {
        // This will make a quote request similar to the previous test but with a very big amount
        // We expect the Bebop Quote to have an aggregate order (split between different mms)
        let token_in = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let token_out = Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap();
        dotenv().expect("Missing .env file");
        let key = std::env::var("BEBOP_KEY").expect("BEBOP_KEY not set");

        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        let common = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[
                (&token_in, "TOKEN_IN", 18),
                (&token_out, "TOKEN_OUT", 18),
            ])),
            min_tvl_usd: 10.0,
        };
        let feed = BebopFeedBuilder::new(common, HashSet::new(), key)
            .quote_timeout(Duration::from_secs(30))
            .origin_address(Bytes::from_str("0x00000000219ab540356cBB839Cbe05303d7705Fa").unwrap())
            .origin_target(router.clone())
            .origin_source("tycho-test".to_string())
            .build()
            .unwrap();

        let amount_in = BigUint::from_str("20_000_000_000").unwrap(); // 20k USDC
        let params = GetAmountOutParams {
            amount_in: amount_in.clone(),
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            sender: router.clone(),
            receiver: router,
        };
        let quote = feed
            .source
            .client
            .request_binding_quote(&params)
            .await
            .unwrap();

        assert_eq!(quote.base_token, token_in);
        assert_eq!(quote.quote_token, token_out);
        assert_eq!(quote.amount_in, amount_in);

        // Assuming the USDC - ONDO price doesn't change too much at the time of running this
        assert!(quote.amount_out > BigUint::from_str("18000000000000000000000").unwrap()); // ~19k ONDO

        // The settlement mode depends on the API account configuration behind BEBOP_KEY:
        // settlement-mode accounts get BebopSettlement.swapAggregate calldata, router-mode
        // accounts get BebopRouter.swap calldata.
        let selector = &quote
            .quote_attributes
            .get("calldata")
            .unwrap()[..4];
        if selector == SWAP_AGGREGATE_SELECTOR {
            let partial_fill_offset_slice = quote
                .quote_attributes
                .get("partial_fill_offset")
                .unwrap()
                .as_ref();
            let mut partial_fill_offset_array = [0u8; 8];
            partial_fill_offset_array.copy_from_slice(partial_fill_offset_slice);

            // This is the only attribute that is significantly different for the Single and
            // Aggregate Order
            assert_eq!(u64::from_be_bytes(partial_fill_offset_array), 2);
        } else {
            assert_eq!(selector, ROUTER_SWAP_SELECTOR);
        }
    }
}
