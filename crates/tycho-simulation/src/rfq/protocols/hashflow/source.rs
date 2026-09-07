use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tracing::debug;
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::{pair_component, pair_component_id},
        tvl, Book, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    protocol::models::ProtocolComponent,
    rfq::protocols::hashflow::{
        client::HashflowClient, models::HashflowMarketMakerLevels, state::HashflowState,
        PROTOCOL_SYSTEM,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Hashflow book per poll: every market maker's levels between the
/// configured tokens whose normalized TVL clears the threshold, as simulate-ready books.
#[derive(Clone, Debug)]
pub struct HashflowBookSource {
    pub common: BookFeedConfig,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// Requests binding quotes; shared with every state this source emits.
    pub client: Arc<HashflowClient>,
}

impl HashflowBookSource {
    /// The TVL in USD quote-token units, converted through every pair the response priced —
    /// each market maker's level set is one such pair.
    fn normalize_tvl(
        &self,
        raw_tvl: f64,
        quote_token: &Bytes,
        levels_by_mm: &HashMap<String, Vec<HashflowMarketMakerLevels>>,
    ) -> Option<f64> {
        tvl::in_usd_quote_tokens(
            raw_tvl,
            quote_token,
            &self.usd_quote_tokens,
            levels_by_mm
                .values()
                .flatten()
                .map(|mm_level| {
                    (&mm_level.pair.base_token, &mm_level.pair.quote_token, &mm_level.levels)
                }),
        )
    }

    /// Builds the simulation component and state for one streamed pair. The state shares this
    /// source's client, so binding quotes carry the feed's full configuration.
    fn build_book(
        &self,
        component_id: Bytes,
        base_token: Token,
        quote_token: Token,
        mm_name: String,
        mm_level: HashflowMarketMakerLevels,
    ) -> (ProtocolComponent, HashflowState) {
        let state = HashflowState {
            base_token: base_token.clone(),
            quote_token: quote_token.clone(),
            levels: mm_level,
            market_maker: mm_name,
            client: Arc::clone(&self.client),
        };
        let component = pair_component(
            component_id,
            PROTOCOL_SYSTEM,
            "hashflow_pool",
            self.common.chain,
            base_token,
            quote_token,
        );
        (component, state)
    }
}

impl HashflowBookSource {
    /// Both requests (market makers, then their levels) count as one poll.
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
        let market_makers = self
            .client
            .fetch_market_makers()
            .await?;
        let levels_by_mm = self
            .client
            .fetch_price_levels(&market_makers)
            .await?;
        debug!(market_makers = levels_by_mm.len(), "fetched price levels");

        let mut books = HashMap::new();
        for (mm_name, mm_levels) in levels_by_mm.iter() {
            for mm_level in mm_levels {
                let base_bytes = &mm_level.pair.base_token;
                let quote_bytes = &mm_level.pair.quote_token;
                let Some((base_token, quote_token)) = self
                    .common
                    .pair_tokens(base_bytes, quote_bytes)
                else {
                    continue;
                };

                // Hashflow's levels price one direction, so the reverse pair is a different book.
                let component_id = pair_component_id(PROTOCOL_SYSTEM, base_bytes, quote_bytes);
                let tvl = mm_level.levels.notional();
                let Some(normalized_tvl) = self.normalize_tvl(tvl, quote_bytes, &levels_by_mm)
                else {
                    debug!(
                        %component_id,
                        market_maker = %mm_name,
                        "skipping pair, no levels price its quote token in a USD quote token"
                    );
                    continue;
                };

                if !self
                    .common
                    .clears_min_tvl(normalized_tvl, &component_id)
                {
                    continue;
                }

                // `levels_by_mm` stays borrowed for the TVL normalization lookups above.
                let book_key = component_id.to_string();
                let (component, state) = self.build_book(
                    component_id,
                    base_token.clone(),
                    quote_token.clone(),
                    mm_name.clone(),
                    mm_level.clone(),
                );
                books
                    .insert(book_key, Book { component, state: Arc::new(state), updated_at: None });
            }
        }
        Ok(books)
    }
}

impl HttpSource for HashflowBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        self.fetch_books()
            .await
            .map(BookSnapshot::received_now)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        book::{
            levels::{Levels, PriceLevel},
            test_token_map,
        },
        rfq::protocols::hashflow::models::HashflowPair,
        snapshot_feed::http::test_support::spawn_http_server,
    };

    /// A source over an empty token universe with USDC and USDT as the USD quote tokens.
    fn test_source() -> HashflowBookSource {
        source_for("https://hashflow.example", &[], 1.0)
    }

    /// A source whose endpoints live under `endpoint`, serving `tokens` (18 decimals each) above
    /// `min_tvl_usd`, with USDC and USDT as the USD quote tokens.
    fn source_for(endpoint: &str, tokens: &[&Bytes], min_tvl_usd: f64) -> HashflowBookSource {
        let quote_tokens = HashSet::from([
            Bytes::from_str(USDC).unwrap(),
            Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(), // USDT
        ]);
        let entries: Vec<_> = tokens
            .iter()
            .map(|address| (*address, "T", 18))
            .collect();
        HashflowBookSource {
            common: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&entries)),
                min_tvl_usd,
            },
            usd_quote_tokens: Arc::new(quote_tokens),
            client: Arc::new(HashflowClient::new(
                Chain::Ethereum,
                "https://hashflow.example/rfq".to_string(),
                format!("{endpoint}/price-levels"),
                format!("{endpoint}/market-makers"),
                "test_user".to_string(),
                "test_key".to_string(),
                Duration::from_secs(5),
            )),
        }
    }

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const UNPRICED: &str = "0x1234567890123456789012345678901234567890";

    /// The response carries one market maker's WETH/USDC level at 3000.
    #[rstest]
    #[case::usd_quote_token_is_taken_as_is(USDC, 1000.0, Some(1000.0))]
    #[case::other_quote_token_is_converted_through_its_usd_level(WETH, 2.0, Some(6000.0))]
    #[case::quote_token_without_usd_level_has_no_tvl(UNPRICED, 1000.0, None)]
    fn normalize_tvl_converts_into_usd_quote_tokens(
        #[case] quote_token: &str,
        #[case] raw_tvl: f64,
        #[case] expected: Option<f64>,
    ) {
        let source = test_source();
        let weth_usdc = HashflowMarketMakerLevels {
            pair: HashflowPair {
                base_token: Bytes::from_str(WETH).unwrap(),
                quote_token: Bytes::from_str(USDC).unwrap(),
            },
            levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 3000.0 }]).unwrap(),
        };
        let levels = HashMap::from([("test_mm".to_string(), vec![weth_usdc])]);

        let result = source.normalize_tvl(raw_tvl, &Bytes::from_str(quote_token).unwrap(), &levels);

        assert_eq!(result, expected);
    }

    /// Every market maker's level set becomes its own directed book once its TVL, normalized
    /// into a USD quote token, clears the floor; the response carries no timestamps.
    #[tokio::test]
    async fn fetch_books_builds_one_directed_book_per_pair() {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let market_makers = r#"{"marketMakers":["mm1","mm2"]}"#.to_string();
        // mm1: WETH/USDC worth 3000 USD; WBTC/USDC worth 100 USD (below the 1000 USD floor).
        // mm2: WBTC/WETH worth 10 WETH, priced through mm1's WETH/USDC level at 3000.
        let price_levels = format!(
            r#"{{"status":"success","levels":{{
                "mm1":[
                    {{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"1","p":"3000"}}]}},
                    {{"pair":{{"baseToken":"{wbtc}","quoteToken":"{USDC}"}},"levels":[{{"q":"0.1","p":"1000"}}]}}
                ],
                "mm2":[
                    {{"pair":{{"baseToken":"{wbtc}","quoteToken":"{WETH}"}},"levels":[{{"q":"1","p":"10"}}]}}
                ]
            }}}}"#
        );
        let server = spawn_http_server(move |target| {
            if target.starts_with("/market-makers") {
                Some(("200 OK", market_makers.clone()))
            } else {
                Some(("200 OK", price_levels.clone()))
            }
        })
        .await;
        let source = source_for(&server.url(), &[&weth, &usdc, &wbtc], 1000.0);

        let books = source.fetch_books().await.unwrap();

        let weth_usdc = pair_component_id(PROTOCOL_SYSTEM, &weth, &usdc);
        let wbtc_weth = pair_component_id(PROTOCOL_SYSTEM, &wbtc, &weth);
        let wbtc_usdc = pair_component_id(PROTOCOL_SYSTEM, &wbtc, &usdc);
        let key = |id: &Bytes| id.to_string();
        assert_eq!(
            books
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            HashSet::from([key(&weth_usdc), key(&wbtc_weth)]),
            "below-floor {wbtc_usdc} must be absent"
        );
        let book = &books[&key(&wbtc_weth)];
        assert_eq!(book.updated_at, None);
        assert_eq!(book.component.id, wbtc_weth);
        let state = book
            .state
            .as_any()
            .downcast_ref::<HashflowState>()
            .expect("a Hashflow state");
        assert_eq!(state.base_token().address, wbtc);
        assert_eq!(state.quote_token().address, weth);
    }

    /// Hashflow signals failure inside an HTTP 200: a non-success status is a connection error
    /// carrying the API's message, a success without levels a parsing error.
    #[rstest]
    #[case::status_fail_with_message(
        r#"{"status":"fail","error":"rate limited"}"#,
        FeedError::Connection("rate limited".to_string())
    )]
    #[case::success_without_levels(r#"{"status":"success"}"#, FeedError::Parsing(String::new()))]
    #[tokio::test]
    async fn price_levels_status_fail_and_missing_levels_are_errors(
        #[case] body: &'static str,
        #[case] expected: FeedError,
    ) {
        let server = spawn_http_server(move |_| Some(("200 OK", body.to_string()))).await;
        let source = source_for(&server.url(), &[], 1.0);

        let error = source
            .client
            .fetch_price_levels(&["mm1".to_string()])
            .await
            .unwrap_err();

        match (&error, &expected) {
            (FeedError::Connection(msg), FeedError::Connection(text)) => {
                assert!(msg.contains(text), "{msg}")
            }
            (FeedError::Parsing(_), FeedError::Parsing(_)) => {}
            _ => panic!("expected {expected:?}, got {error:?}"),
        }
    }
}
