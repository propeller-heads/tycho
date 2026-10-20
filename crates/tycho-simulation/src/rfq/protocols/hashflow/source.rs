use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tracing::debug;
use tycho_common::Bytes;

use crate::{
    book::{
        all_pairs::{BookCollector, ComponentLayout, MarketMaker, QuoteRule, VenueBook},
        component::maker_component_id,
        tvl, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    rfq::protocols::hashflow::{
        client::HashflowClient,
        models::HashflowMarketMakerLevels,
        state::{HashflowAllPairsState, HashflowState},
        PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Hashflow book per poll: every market maker's levels between the
/// configured tokens whose normalized TVL clears the threshold, one simulate-ready book per maker
/// and pair.
#[derive(Clone, Debug)]
pub struct HashflowBookSource {
    /// The protocol system stamped on this source's components: the venue's own, or its
    /// fallback-router one for a feed built with `with_fallback_router()`.
    pub protocol_system: &'static str,
    pub book_config: BookFeedConfig,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// Requests binding quotes; shared with every state this source emits.
    pub client: Arc<HashflowClient>,
    /// Whether the venue's pairs are published as one component or one each.
    pub component_layout: ComponentLayout<QuoteRule>,
}

impl HashflowBookSource {
    /// The TVL in USD quote-token units, converted through every pair the response priced —
    /// each market maker's level set is one such pair.
    fn normalized_tvl(
        &self,
        raw_tvl: f64,
        quote_token: &Bytes,
        levels_by_mm: &HashMap<String, Vec<HashflowMarketMakerLevels>>,
    ) -> Option<f64> {
        tvl::tvl_in_usd(
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
}

impl HttpSource for HashflowBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// Both requests (market makers, then their levels) count as one poll.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let market_makers = self
            .client
            .fetch_market_makers()
            .await?;
        let levels_by_mm = self
            .client
            .fetch_price_levels(&market_makers)
            .await?;
        debug!(market_makers = levels_by_mm.len(), "fetched price levels");

        let mut collector = BookCollector::new(
            self.protocol_system,
            PROTOCOL_TYPE,
            self.book_config.chain,
            self.component_layout,
        );
        for (mm_name, mm_levels) in levels_by_mm.iter() {
            for mm_level in mm_levels {
                let base_bytes = &mm_level.pair.base_token;
                let quote_bytes = &mm_level.pair.quote_token;
                let Some((base_token, quote_token)) = self
                    .book_config
                    .pair_tokens(base_bytes, quote_bytes)
                else {
                    continue;
                };

                // Hashflow's levels price one direction, so the reverse pair is a different book.
                let component_id =
                    maker_component_id(self.protocol_system, base_bytes, quote_bytes, mm_name);
                let tvl = mm_level.levels.notional();
                let Some(normalized_tvl) = self.normalized_tvl(tvl, quote_bytes, &levels_by_mm)
                else {
                    debug!(
                        %component_id,
                        market_maker = %mm_name,
                        "skipping pair, no levels price its quote token in a USD quote token"
                    );
                    continue;
                };

                if !self
                    .book_config
                    .clears_min_tvl(normalized_tvl, &component_id)
                {
                    continue;
                }

                // `levels_by_mm` stays borrowed for the TVL normalization lookups above.
                let state = HashflowState::new(
                    base_token.clone(),
                    quote_token.clone(),
                    mm_level.clone(),
                    mm_name.clone(),
                    Arc::clone(&self.client),
                );
                collector.push(
                    component_id,
                    VenueBook { signer: MarketMaker(mm_name.clone()), updated_at: None, state },
                );
            }
        }
        Ok(BookSnapshot::received_now(collector.finish::<HashflowAllPairsState>()))
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use rstest::rstest;
    use tycho_common::{models::Chain, simulation::errors::SimulationError};

    use super::*;
    use crate::{
        book::{
            all_pairs::QuoteRule,
            component::PairState,
            decode_swap_directions,
            levels::{Levels, PriceLevel},
            test_token_map,
        },
        rfq::protocols::hashflow::{
            models::HashflowPair, FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM,
        },
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
            component_layout: ComponentLayout::PerPair,
            protocol_system: PROTOCOL_SYSTEM,
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&entries)),
                min_tvl_usd,
            },
            usd_quote_tokens: Arc::new(quote_tokens),
            client: Arc::new(HashflowClient::new(
                Some(endpoint),
                1,
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
    fn normalized_tvl_converts_into_usd_quote_tokens(
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

        let result =
            source.normalized_tvl(raw_tvl, &Bytes::from_str(quote_token).unwrap(), &levels);

        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::per_pair(
        ComponentLayout::PerPair,
        // "fallback:rfq:hashflow" in ASCII, then WETH, USDC and the maker's name.
        "0x66616c6c6261636b3a7266713a68617368666c6f77\
           c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2\
           a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\
           6d6d31"
    )]
    #[case::all_pairs(
        ComponentLayout::AllPairs(QuoteRule::OncePerMaker),
        // The venue-wide component is the protocol system and nothing else.
        "0x66616c6c6261636b3a7266713a68617368666c6f77"
    )]
    #[tokio::test]
    async fn a_fallback_source_labels_its_components_for_the_fallback_router(
        #[case] component_layout: ComponentLayout<QuoteRule>,
        #[case] expected_id: &str,
    ) {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let market_makers = r#"{"marketMakers":["mm1"]}"#.to_string();
        let price_levels = format!(
            r#"{{"status":"success","levels":{{
                "mm1":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"1","p":"3000"}}]}}]
            }}}}"#
        );
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/taker/v3/market-makers")
            {
                Some(("200 OK", market_makers.clone()))
            } else {
                Some(("200 OK", price_levels.clone()))
            }
        })
        .await;
        let source = HashflowBookSource {
            protocol_system: FALLBACK_PROTOCOL_SYSTEM,
            component_layout,
            ..source_for(&server.url(), &[&weth, &usdc], 0.0)
        };

        let books = source.fetch().await.unwrap().books;

        let book = books.values().next().expect("one book");
        assert_eq!(book.component.protocol_system, "fallback:rfq:hashflow");
        assert_eq!(book.component.id.to_string(), expected_id);
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
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/taker/v3/market-makers")
            {
                Some(("200 OK", market_makers.clone()))
            } else {
                Some(("200 OK", price_levels.clone()))
            }
        })
        .await;
        let source = source_for(&server.url(), &[&weth, &usdc, &wbtc], 1000.0);

        let books = source.fetch().await.unwrap().books;

        let weth_usdc = maker_component_id(PROTOCOL_SYSTEM, &weth, &usdc, "mm1");
        let wbtc_weth = maker_component_id(PROTOCOL_SYSTEM, &wbtc, &weth, "mm2");
        let wbtc_usdc = maker_component_id(PROTOCOL_SYSTEM, &wbtc, &usdc, "mm1");
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
        assert_eq!(
            decode_swap_directions(&book.component.static_attributes["swap_directions"]).unwrap(),
            vec![(wbtc.clone(), weth.clone())],
            "a maker's levels sell the base token only, which the component has to say"
        );
    }

    /// Two makers quoting the same pair are two books, each with its own maker and ladder.
    #[tokio::test]
    async fn fetch_books_keeps_every_market_maker_of_a_pair() {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let market_makers = r#"{"marketMakers":["mm1","mm2"]}"#.to_string();
        let price_levels = format!(
            r#"{{"status":"success","levels":{{
                "mm1":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"1","p":"3000"}}]}}],
                "mm2":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"2","p":"2990"}}]}}]
            }}}}"#
        );
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/taker/v3/market-makers")
            {
                Some(("200 OK", market_makers.clone()))
            } else {
                Some(("200 OK", price_levels.clone()))
            }
        })
        .await;
        let source = source_for(&server.url(), &[&weth, &usdc], 1.0);

        let books = source.fetch().await.unwrap().books;

        let makers_and_ladders: HashMap<_, _> = books
            .values()
            .map(|book| {
                let state = book
                    .state
                    .as_any()
                    .downcast_ref::<HashflowState>()
                    .expect("a Hashflow state");
                assert_eq!(
                    book.component.id,
                    maker_component_id(PROTOCOL_SYSTEM, &weth, &usdc, &state.market_maker)
                );
                (state.market_maker.clone(), state.levels.levels.clone())
            })
            .collect();
        assert_eq!(
            makers_and_ladders,
            HashMap::from([
                (
                    "mm1".to_string(),
                    Levels::new(vec![PriceLevel { quantity: 1.0, price: 3000.0 }]).unwrap()
                ),
                (
                    "mm2".to_string(),
                    Levels::new(vec![PriceLevel { quantity: 2.0, price: 2990.0 }]).unwrap()
                ),
            ])
        );
    }

    /// Hashflow signals failure inside an HTTP 200: a non-success status is a connection error
    /// carrying the API's message, a success without levels a parsing error.
    #[rstest]
    #[case::status_fail_with_message(
        r#"{"status":"fail","error":{"code":429,"message":"rate limited"}}"#,
        FeedError::Connection("rate limited (code 429)".to_string())
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

    /// Published as one component, every market maker's levels stay their own book, and the
    /// venue's rule says how many of them one route may quote: every maker once, or the venue
    /// once whichever maker answers.
    #[rstest]
    #[case::once_per_maker(QuoteRule::OncePerMaker, "once_per_maker", 2)]
    #[case::once_per_venue(QuoteRule::OncePerVenue, "once_per_venue", 1)]
    #[tokio::test]
    async fn fetch_publishes_every_maker_under_one_component(
        #[case] rule: QuoteRule,
        #[case] attribute: &str,
        #[case] quotable_swaps: usize,
    ) {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let market_makers = r#"{"marketMakers":["mm1","mm2"]}"#.to_string();
        // Both makers quote one WETH of WETH/USDC depth, mm1 a dollar better.
        let price_levels = format!(
            r#"{{"status":"success","levels":{{
                "mm1":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"1","p":"3000"}}]}}],
                "mm2":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"1","p":"2999"}}]}}]
            }}}}"#
        );
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/taker/v3/market-makers")
            {
                Some(("200 OK", market_makers.clone()))
            } else {
                Some(("200 OK", price_levels.clone()))
            }
        })
        .await;
        let source = HashflowBookSource {
            component_layout: ComponentLayout::AllPairs(rule),
            ..source_for(&server.url(), &[&weth, &usdc], 1000.0)
        };

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.len(), 1, "one component: {:?}", books.keys());
        let book = books.values().next().unwrap();
        // The venue's protocol system in ASCII, which is the whole id.
        assert_eq!(book.component.id.to_string(), "0x7266713a68617368666c6f77");
        assert_eq!(
            decode_swap_directions(&book.component.static_attributes["swap_directions"]).unwrap(),
            vec![(weth.clone(), usdc.clone())],
            "Hashflow's levels price one direction"
        );
        assert_eq!(
            book.component.static_attributes["quote_rule"],
            Bytes::from(attribute.as_bytes())
        );

        let tokens = &source.book_config.tokens;
        // Hashflow's first level is the minimum tradable size, so the swap is one whole WETH.
        let swap = |state: &dyn tycho_common::simulation::protocol_sim::ProtocolSim| {
            state.get_amount_out(
                num_bigint::BigUint::from(10u64).pow(18),
                &tokens[&weth],
                &tokens[&usdc],
            )
        };
        let mut state = book.state.clone_box();
        let mut amounts = Vec::new();
        while let Ok(result) = swap(state.as_ref()) {
            amounts.push(result.amount.clone());
            state = result.new_state;
        }
        assert_eq!(amounts.len(), quotable_swaps, "swaps the rule allows: {amounts:?}");
        if let [first, second] = amounts.as_slice() {
            assert!(first > second, "the better maker quotes first");
        }
    }

    /// A maker declines an amount below its first level, so the venue's floor under one
    /// component is the smallest of its makers' — and what it refuses below, it names.
    #[tokio::test]
    async fn the_venue_floor_is_the_smallest_makers_minimum() {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let market_makers = r#"{"marketMakers":["deep_mm","small_mm"]}"#.to_string();
        // deep_mm takes two WETH at a time, small_mm takes one.
        let price_levels = format!(
            r#"{{"status":"success","levels":{{
                "deep_mm":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"2","p":"3000"}}]}}],
                "small_mm":[{{"pair":{{"baseToken":"{WETH}","quoteToken":"{USDC}"}},"levels":[{{"q":"1","p":"2999"}}]}}]
            }}}}"#
        );
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/taker/v3/market-makers")
            {
                Some(("200 OK", market_makers.clone()))
            } else {
                Some(("200 OK", price_levels.clone()))
            }
        })
        .await;
        let source = HashflowBookSource {
            component_layout: ComponentLayout::AllPairs(QuoteRule::OncePerMaker),
            ..source_for(&server.url(), &[&weth, &usdc], 1000.0)
        };

        let books = source.fetch().await.unwrap().books;
        let state = &books.values().next().unwrap().state;
        let tokens = &source.book_config.tokens;
        let swap = |whole_weth: u64| {
            state.get_amount_out(
                num_bigint::BigUint::from(whole_weth) * num_bigint::BigUint::from(10u64).pow(17),
                &tokens[&weth],
                &tokens[&usdc],
            )
        };

        // Half a WETH is under every maker's floor, one WETH is over small_mm's.
        match swap(5) {
            Err(SimulationError::RecoverableError(message)) => assert_eq!(
                message,
                "Amount below minimum. Input amount: 500000000000000000, min amount: \
                 1000000000000000000 (both in base units)"
            ),
            other => panic!("expected the venue's floor, got {other:?}"),
        }
        assert!(swap(10).is_ok(), "one WETH is small_mm's minimum");
    }
}
