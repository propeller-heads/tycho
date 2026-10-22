use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use chrono::DateTime;
use tracing::debug;
use tycho_common::Bytes;

use crate::{
    book::{
        all_pairs::{BookCollector, ComponentLayout, MarketMaker, QuoteRule, VenueBook},
        component::maker_component_id,
        tvl, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    rfq::protocols::liquorice::{
        client::LiquoriceClient,
        models::LiquoriceTokenPairPrice,
        state::{LiquoriceAllPairsState, LiquoriceState},
        PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Liquorice book per poll: one simulate-ready book per market maker and
/// pair between the configured tokens, for every maker whose normalized TVL clears the
/// threshold. A binding quote comes from the maker it is asked of, so a maker's ladder is as
/// far as one swap can fill.
#[derive(Clone, Debug)]
pub struct LiquoriceBookSource {
    /// The protocol system stamped on this source's components: the venue's own, or its
    /// fallback-router one for a feed built with `with_fallback_router()`.
    pub protocol_system: &'static str,
    pub book_config: BookFeedConfig,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// Requests binding quotes; shared with every state this source emits.
    pub client: Arc<LiquoriceClient>,
    /// Whether the venue's pairs are published as one component or one each.
    pub component_layout: ComponentLayout<QuoteRule>,
}

impl LiquoriceBookSource {
    /// The TVL in USD quote-token units, converted through every pair the response priced —
    /// each market maker quotes one such pair per entry.
    fn normalized_tvl(
        &self,
        raw_tvl: f64,
        quote_token: &Bytes,
        prices_by_mm: &HashMap<String, Vec<LiquoriceTokenPairPrice>>,
    ) -> Option<f64> {
        tvl::tvl_in_usd(
            raw_tvl,
            quote_token,
            &self.usd_quote_tokens,
            prices_by_mm
                .values()
                .flatten()
                .map(|price| (&price.base_token, &price.quote_token, &price.levels)),
        )
    }
}

impl HttpSource for LiquoriceBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let prices_by_mm = self.client.fetch_price_levels().await?;
        debug!(market_makers = prices_by_mm.len(), "fetched price levels");

        let mut collector = BookCollector::new(
            self.protocol_system,
            PROTOCOL_TYPE,
            self.book_config.chain,
            self.component_layout,
        );
        for (mm_name, token_pair_prices) in prices_by_mm.iter() {
            for token_pair_price in token_pair_prices {
                let base_bytes = &token_pair_price.base_token;
                let quote_bytes = &token_pair_price.quote_token;
                let Some((base_token, quote_token)) = self
                    .book_config
                    .pair_tokens(base_bytes, quote_bytes)
                else {
                    continue;
                };

                let component_id =
                    maker_component_id(self.protocol_system, base_bytes, quote_bytes, mm_name);
                let tvl = token_pair_price.levels.notional();
                let Some(normalized_tvl) = self.normalized_tvl(tvl, quote_bytes, &prices_by_mm)
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

                // Liquorice reports `updatedAt` per market maker, in milliseconds.
                let updated_at = token_pair_price
                    .updated_at
                    .and_then(|millis| DateTime::from_timestamp_millis(millis as i64));
                let state = LiquoriceState::new(
                    base_token.clone(),
                    quote_token.clone(),
                    token_pair_price.clone(),
                    mm_name.clone(),
                    Arc::clone(&self.client),
                );
                collector.push(
                    component_id,
                    VenueBook { signer: MarketMaker(mm_name.clone()), updated_at, state },
                );
            }
        }
        Ok(BookSnapshot::received_now(collector.finish::<LiquoriceAllPairsState>()))
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
            all_pairs::QuoteRule,
            decode_swap_directions,
            levels::{Levels, PriceLevel},
            test_token_map,
        },
        rfq::protocols::liquorice::{FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM},
        snapshot_feed::http::test_support::spawn_http_server,
    };

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

    /// A source on Ethereum with USDC and USDT as the USD quote tokens and an empty universe.
    fn test_source() -> LiquoriceBookSource {
        source_for("https://api.liquorice.tech/v1/solver", &[], 1.0)
    }

    /// A source polling `endpoint`, serving `tokens` (18 decimals each) above `min_tvl_usd`,
    /// with USDC and USDT as the USD quote tokens.
    fn source_for(endpoint: &str, tokens: &[&Bytes], min_tvl_usd: f64) -> LiquoriceBookSource {
        let usd_quote_tokens = HashSet::from([
            Bytes::from_str(USDC).unwrap(),
            Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(), // USDT
        ]);
        let entries: Vec<_> = tokens
            .iter()
            .map(|address| (*address, "T", 18))
            .collect();
        LiquoriceBookSource {
            protocol_system: PROTOCOL_SYSTEM,
            component_layout: ComponentLayout::PerPair,
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&entries)),
                min_tvl_usd,
            },
            usd_quote_tokens: Arc::new(usd_quote_tokens),
            client: Arc::new(LiquoriceClient::new(
                Some(endpoint),
                Chain::Ethereum.id(),
                "test_solver".to_string(),
                "test_key".to_string(),
                Duration::from_secs(5),
                300,
            )),
        }
    }

    /// TVL quoted in a USD quote token is taken as is; TVL in another token is converted through
    /// a maker's levels that price that token in a USD quote token, or is not normalizable at all.
    #[rstest]
    #[case::usd_quote_token_needs_no_conversion(
        "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        false,
        Some(1000.0)
    )]
    #[case::converted_through_a_usd_priced_pair(
        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        true,
        Some(3_000_000.0)
    )]
    #[case::no_conversion_pair_available("0x1234567890123456789012345678901234567890", true, None)]
    fn normalized_tvl(
        #[case] quote_token: &str,
        #[case] with_weth_usdc_levels: bool,
        #[case] expected: Option<f64>,
    ) {
        let source = test_source();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let mut prices = HashMap::new();
        if with_weth_usdc_levels {
            // 1 WETH = 3000 USDC.
            prices.insert(
                "test_mm".to_string(),
                vec![LiquoriceTokenPairPrice {
                    base_token: weth,
                    quote_token: usdc,
                    levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 3000.0 }]).unwrap(),
                    updated_at: None,
                }],
            );
        }

        let result = source.normalized_tvl(1000.0, &Bytes::from_str(quote_token).unwrap(), &prices);

        assert_eq!(result, expected);
    }

    /// Each maker pricing a pair above the floor is a book of its own, carrying that maker's
    /// own `updatedAt` (milliseconds); a maker below the floor is left out.
    #[tokio::test]
    async fn fetch_books_publishes_one_book_per_maker_and_pair() {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let newest_ms: u64 = 1_769_707_860_675;
        // fresh_mm and old_mm each hold 3000 USD of WETH/USDC; tiny_mm holds 30 USD, below the
        // 1000 USD floor.
        let body = format!(
            r#"{{"prices":{{
                "fresh_mm":[{{"baseToken":"{WETH}","quoteToken":"{USDC}","levels":[["3000","1"]],"updatedAt":{newest_ms}}}],
                "old_mm":[{{"baseToken":"{WETH}","quoteToken":"{USDC}","levels":[["2999","1"]],"updatedAt":{}}}],
                "tiny_mm":[{{"baseToken":"{WETH}","quoteToken":"{USDC}","levels":[["3000","0.01"]]}}]
            }}}}"#,
            newest_ms - 60_000
        );
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let source = source_for(&server.url(), &[&weth, &usdc], 1000.0);

        let books = source.fetch().await.unwrap().books;

        let id = |maker: &str| maker_component_id(PROTOCOL_SYSTEM, &weth, &usdc, maker).to_string();
        let mut ids = books
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec![id("fresh_mm"), id("old_mm")]);
        assert_eq!(
            books[&id("fresh_mm")].updated_at,
            DateTime::from_timestamp_millis(newest_ms as i64)
        );
        assert_eq!(
            books[&id("old_mm")].updated_at,
            DateTime::from_timestamp_millis((newest_ms - 60_000) as i64)
        );
        let expected = LiquoriceState::new(
            source.book_config.tokens[&weth].clone(),
            source.book_config.tokens[&usdc].clone(),
            LiquoriceTokenPairPrice {
                base_token: weth.clone(),
                quote_token: usdc.clone(),
                levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 3000.0 }]).unwrap(),
                updated_at: Some(newest_ms),
            },
            "fresh_mm".to_string(),
            Arc::clone(&source.client),
        );
        assert!(
            books[&id("fresh_mm")]
                .state
                .eq(&expected),
            "the book holds fresh_mm's own ladder"
        );
    }

    #[rstest]
    #[case::per_pair(ComponentLayout::PerPair)]
    #[case::all_pairs(ComponentLayout::AllPairs(QuoteRule::OncePerMaker))]
    #[tokio::test]
    async fn a_fallback_source_labels_its_components_for_the_fallback_router(
        #[case] component_layout: ComponentLayout<QuoteRule>,
    ) {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let body = format!(
            r#"{{"prices":{{
                "test_mm":[{{"baseToken":"{WETH}","quoteToken":"{USDC}","levels":[["3000","1"]]}}]
            }}}}"#
        );
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let source = LiquoriceBookSource {
            protocol_system: FALLBACK_PROTOCOL_SYSTEM,
            component_layout,
            ..source_for(&server.url(), &[&weth, &usdc], 0.0)
        };

        let books = source.fetch().await.unwrap().books;

        let book = books.values().next().expect("one book");
        assert_eq!(book.component.protocol_system, "fallback:rfq:liquorice");
        let expected_id = match component_layout {
            ComponentLayout::PerPair => {
                maker_component_id(FALLBACK_PROTOCOL_SYSTEM, &weth, &usdc, "test_mm")
            }
            ComponentLayout::AllPairs(_) => Bytes::from(FALLBACK_PROTOCOL_SYSTEM.as_bytes()),
        };
        assert_eq!(book.component.id, expected_id);
    }

    /// Published as one component, each market maker pricing a pair becomes its own book: a
    /// route takes one quote per maker, so two makers on one pair serve two swaps and a third
    /// finds none left.
    #[tokio::test]
    async fn fetch_splits_the_makers_of_a_pair_under_one_component() {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        // Both makers hold 3000 USD of WETH/USDC, fresh_mm a dollar better.
        let body = format!(
            r#"{{"prices":{{
                "fresh_mm":[{{"baseToken":"{WETH}","quoteToken":"{USDC}","levels":[["3000","1"]]}}],
                "old_mm":[{{"baseToken":"{WETH}","quoteToken":"{USDC}","levels":[["2999","1"]]}}]
            }}}}"#
        );
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let source = LiquoriceBookSource {
            component_layout: ComponentLayout::AllPairs(QuoteRule::OncePerMaker),
            ..source_for(&server.url(), &[&weth, &usdc], 1000.0)
        };

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.len(), 1, "one component: {:?}", books.keys());
        let book = books.values().next().unwrap();
        // The venue's protocol system in ASCII, which is the whole id.
        assert_eq!(book.component.id.to_string(), "0x7266713a6c6971756f72696365");
        assert_eq!(
            decode_swap_directions(&book.component.static_attributes["swap_directions"]).unwrap(),
            vec![(weth.clone(), usdc.clone())],
            "Liquorice's levels price one direction"
        );

        let tokens = &source.book_config.tokens;
        let swap = |state: &dyn tycho_common::simulation::protocol_sim::ProtocolSim| {
            state.get_amount_out(
                num_bigint::BigUint::from(10u64).pow(17),
                &tokens[&weth],
                &tokens[&usdc],
            )
        };
        let best = swap(book.state.as_ref()).unwrap();
        let second = swap(best.new_state.as_ref()).unwrap();
        assert!(best.amount > second.amount, "the better maker quotes first");
        assert!(swap(second.new_state.as_ref()).is_err(), "two makers quote two swaps and no more");
    }
}
