use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use chrono::DateTime;
use tracing::debug;
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::{pair_component, pair_component_id},
        tvl, Book, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    protocol::models::ProtocolComponent,
    rfq::protocols::liquorice::{
        client::LiquoriceClient, models::LiquoriceTokenPairPrice, state::LiquoriceState,
        PROTOCOL_SYSTEM,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Liquorice book per poll: for each pair between the configured tokens,
/// the market makers whose normalized TVL clears the threshold, grouped into one simulate-ready
/// pair.
#[derive(Clone, Debug)]
pub struct LiquoriceBookSource {
    pub book_config: BookFeedConfig,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// Requests binding quotes; shared with every state this source emits.
    pub client: Arc<LiquoriceClient>,
}

impl LiquoriceBookSource {
    /// The TVL in USD quote-token units, converted through every pair the response priced —
    /// each market maker quotes one such pair per entry.
    fn normalize_tvl(
        &self,
        raw_tvl: f64,
        quote_token: &Bytes,
        prices_by_mm: &HashMap<String, Vec<LiquoriceTokenPairPrice>>,
    ) -> Option<f64> {
        tvl::in_usd_quote_tokens(
            raw_tvl,
            quote_token,
            &self.usd_quote_tokens,
            prices_by_mm
                .values()
                .flatten()
                .map(|price| (&price.base_token, &price.quote_token, &price.levels)),
        )
    }

    /// Builds the simulation component and state for one streamed pair. The state shares this
    /// source's client, so binding quotes carry the feed's full configuration.
    fn build_book(
        &self,
        component_id: Bytes,
        base_token: Token,
        quote_token: Token,
        prices_by_mm: HashMap<String, LiquoriceTokenPairPrice>,
    ) -> (ProtocolComponent, LiquoriceState) {
        let state = LiquoriceState {
            base_token: base_token.clone(),
            quote_token: quote_token.clone(),
            prices_by_mm,
            client: Arc::clone(&self.client),
        };
        let component = pair_component(
            component_id,
            PROTOCOL_SYSTEM,
            "liquorice_pool",
            self.book_config.chain,
            base_token,
            quote_token,
        );
        (component, state)
    }
}

impl LiquoriceBookSource {
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
        let prices_by_mm = self.client.fetch_price_levels().await?;
        debug!(market_makers = prices_by_mm.len(), "fetched price levels");

        // Group qualifying MMs by token pair
        let mut pair_mm_prices: HashMap<(Bytes, Bytes), HashMap<String, LiquoriceTokenPairPrice>> =
            HashMap::new();
        for (mm_name, token_pair_prices) in prices_by_mm.iter() {
            for token_pair_price in token_pair_prices {
                let base_token = &token_pair_price.base_token;
                let quote_token = &token_pair_price.quote_token;
                if self
                    .book_config
                    .pair_tokens(base_token, quote_token)
                    .is_none()
                {
                    continue;
                }

                let tvl = token_pair_price.levels.notional();
                let Some(normalized_tvl) = self.normalize_tvl(tvl, quote_token, &prices_by_mm)
                else {
                    debug!(
                        market_maker = %mm_name,
                        base = %base_token,
                        quote = %quote_token,
                        "skipping pair, no levels price its quote token in a USD quote token"
                    );
                    continue;
                };
                if !self.book_config.clears_min_tvl(
                    normalized_tvl,
                    format!("MM {mm_name} for pair {base_token}/{quote_token}"),
                ) {
                    continue;
                }

                pair_mm_prices
                    .entry((base_token.clone(), quote_token.clone()))
                    .or_default()
                    .insert(mm_name.clone(), token_pair_price.clone());
            }
        }

        let mut books = HashMap::new();
        for ((base_bytes, quote_bytes), mm_prices) in pair_mm_prices {
            let Some((base_token, quote_token)) = self
                .book_config
                .pair_tokens(&base_bytes, &quote_bytes)
            else {
                continue;
            };
            // Liquorice's levels price one direction, so the reverse pair is a different book.
            let component_id = pair_component_id(PROTOCOL_SYSTEM, &base_bytes, &quote_bytes);

            // Liquorice reports `updatedAt` per market maker in milliseconds; the book is as
            // fresh as its most recently updated maker.
            let updated_at = mm_prices
                .values()
                .filter_map(|price| price.updated_at)
                .max()
                .and_then(|millis| DateTime::from_timestamp_millis(millis as i64));
            let book_key = component_id.to_string();
            let (component, state) =
                self.build_book(component_id, base_token.clone(), quote_token.clone(), mm_prices);
            books.insert(book_key, Book { component, state: Arc::new(state), updated_at });
        }
        Ok(books)
    }
}

impl HttpSource for LiquoriceBookSource {
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
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&entries)),
                min_tvl_usd,
            },
            usd_quote_tokens: Arc::new(usd_quote_tokens),
            client: Arc::new(LiquoriceClient::new(
                Chain::Ethereum,
                "https://api.liquorice.tech/v1/solver/rfq".to_string(),
                format!("{endpoint}/price-levels"),
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
    fn normalize_tvl(
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

        let result = source.normalize_tvl(1000.0, &Bytes::from_str(quote_token).unwrap(), &prices);

        assert_eq!(result, expected);
    }

    /// The makers pricing one pair above the floor form one directed book whose `updated_at` is
    /// the newest maker's `updatedAt` (milliseconds); a maker below the floor is left out.
    #[tokio::test]
    async fn fetch_books_groups_makers_per_pair_and_stamps_newest_update() {
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

        let books = source.fetch_books().await.unwrap();

        let component_id = pair_component_id(PROTOCOL_SYSTEM, &weth, &usdc);
        assert_eq!(books.len(), 1, "one book for the one pair: {:?}", books.keys());
        let book = &books[&component_id.to_string()];
        assert_eq!(book.updated_at, DateTime::from_timestamp_millis(newest_ms as i64));
        let level = |price: f64, updated_at: u64| LiquoriceTokenPairPrice {
            base_token: weth.clone(),
            quote_token: usdc.clone(),
            levels: Levels::new(vec![PriceLevel { quantity: 1.0, price }]).unwrap(),
            updated_at: Some(updated_at),
        };
        let expected = LiquoriceState {
            base_token: source.book_config.tokens[&weth].clone(),
            quote_token: source.book_config.tokens[&usdc].clone(),
            prices_by_mm: HashMap::from([
                ("fresh_mm".to_string(), level(3000.0, newest_ms)),
                ("old_mm".to_string(), level(2999.0, newest_ms - 60_000)),
            ]),
            client: Arc::clone(&source.client),
        };
        assert!(book.state.eq(&expected), "state must hold exactly fresh_mm and old_mm");
    }
}
