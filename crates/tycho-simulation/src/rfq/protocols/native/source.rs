use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use reqwest::Client;
use tracing::debug;
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::{pair_component, unordered_pair_component_id},
        dedup::group_by_pair,
        errors::FeedError,
        feed_loops::HttpBookSource,
        levels::Levels,
        models::{Book, CommonConfig},
    },
    protocol::models::ProtocolComponent,
    rfq::protocols::native::{
        client::NativeClient,
        models::{
            NativeApiErrorResponse, NativeOrderbookEntry, NativeOrderbookSide, NativePriceData,
            NativeSupportedChain,
        },
        state::NativeState,
        PROTOCOL_SYSTEM,
    },
};

#[derive(Default)]
struct AggregatedLevels {
    levels: Levels,
    // Maximum atomic minimum_in_base among the Native entries contributing these levels.
    minimum: f64,
}

impl AggregatedLevels {
    fn extend(&mut self, levels: Levels, minimum: f64) {
        self.levels.extend(levels);
        self.minimum = self.minimum.max(minimum);
    }
}

/// Fetches one complete Native Relay book per poll: the aggregated orderbook, grouped into one
/// two-sided book per pair, filtered to the configured tokens and the USD TVL threshold.
#[derive(Clone, Debug)]
pub struct NativeBookSource {
    pub common: CommonConfig,
    pub chain: NativeSupportedChain,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    pub orderbook_endpoint: String,
    /// Requests binding quotes; shared with every state this source emits. Its key also
    /// authenticates the orderbook poll.
    pub client: Arc<NativeClient>,
    pub http: Client,
}

impl NativeBookSource {
    // Native API error codes:
    // <https://docs.native.org/native-dev/build-with-native/swap-aggregators/firmquote-swap-apis/miscellaneous/error-handling#error-codes>
    fn orderbook_api_error(error: &NativeApiErrorResponse) -> FeedError {
        let message = format!("Native API error {}: {}", error.code, error.message);
        match error.code {
            // An invalid key never recovers by polling again.
            201001 => FeedError::FatalError(message),
            _ => FeedError::ConnectionError(message),
        }
    }

    async fn fetch_orderbook(&self) -> Result<Vec<NativeOrderbookEntry>, FeedError> {
        let response = self
            .http
            .get(&self.orderbook_endpoint)
            // `showNative` is not boolean: its value selects the address used for native-token
            // books. Request address(0) so the response matches Tycho's internal representation.
            .query(&[("chain", self.chain.as_str()), ("showNative", "0x0")])
            .header("accept", "application/json")
            .header("apikey", self.client.api_key())
            .send()
            .await
            .map_err(|e| FeedError::ConnectionError(e.to_string()))?;

        let status = response.status();
        let response_body = response
            .bytes()
            .await
            .map_err(|e| FeedError::ConnectionError(e.to_string()))?;

        // Native can return an API error envelope with HTTP 200.
        if let Ok(api_error) = serde_json::from_slice::<NativeApiErrorResponse>(&response_body) {
            return Err(Self::orderbook_api_error(&api_error));
        }

        if !status.is_success() {
            return Err(FeedError::ConnectionError(format!(
                "Native Relay orderbook HTTP error {}: {}",
                status,
                String::from_utf8_lossy(&response_body)
            )));
        }

        serde_json::from_slice(&response_body).map_err(|e| {
            FeedError::ParsingError(format!("Failed to parse Native Relay orderbook: {e}"))
        })
    }

    /// Merges the directional orderbook entries into one two-sided book per pair, keyed by a
    /// component id derived from the address-sorted pair so it stays stable whichever orientation
    /// Native publishes in a given poll.
    fn group_orderbook(
        &self,
        entries: Vec<NativeOrderbookEntry>,
    ) -> HashMap<String, NativePriceData> {
        let groups = group_by_pair(
            entries,
            |entry| (entry.base_address.clone(), entry.quote_address.clone()),
            &self.usd_quote_tokens,
        );

        let mut books = HashMap::new();
        for group in groups.into_values() {
            let (base, quote) = (group.base.clone(), group.quote.clone());
            let mut direct_bids = AggregatedLevels::default();
            let mut direct_asks = AggregatedLevels::default();
            let mut mirrored_bids = AggregatedLevels::default();
            let mut mirrored_asks = AggregatedLevels::default();

            // Native's minimum_in_base is always denominated in entry.base_address. For a bid the
            // taker sells base, so it is an input minimum; for an ask the taker receives base, so
            // it is an output minimum. Mirroring swaps bid/ask and remaps that minimum into the
            // canonical direction.
            for entry in group.entries {
                // An entry that carried only placeholder levels must not suppress usable mirrored
                // liquidity for the same side.
                if entry.levels.is_empty() {
                    continue
                }

                if entry.base_address == base && entry.quote_address == quote {
                    match entry.side {
                        NativeOrderbookSide::Bid => {
                            direct_bids.extend(entry.levels, entry.minimum_in_base)
                        }
                        NativeOrderbookSide::Ask => {
                            direct_asks.extend(entry.levels, entry.minimum_in_base)
                        }
                    }
                } else {
                    let levels = entry.levels.invert();
                    match entry.side {
                        NativeOrderbookSide::Bid => {
                            mirrored_asks.extend(levels, entry.minimum_in_base)
                        }
                        NativeOrderbookSide::Ask => {
                            mirrored_bids.extend(levels, entry.minimum_in_base)
                        }
                    }
                }
            }

            // Use mirrored levels only when direct ones are absent to avoid double-counting. Keep
            // the minimum from the selected representation so discarded levels cannot constrain
            // the surviving side.
            let (bids, minimum_in_base, minimum_out_quote) = if direct_bids.levels.is_empty() {
                (mirrored_bids.levels, 0.0, mirrored_bids.minimum)
            } else {
                (direct_bids.levels, direct_bids.minimum, 0.0)
            };
            let (asks, minimum_in_quote, minimum_out_base) = if direct_asks.levels.is_empty() {
                (mirrored_asks.levels, mirrored_asks.minimum, 0.0)
            } else {
                (direct_asks.levels, 0.0, direct_asks.minimum)
            };
            let component_id = unordered_pair_component_id("native", &group.token0, &group.token1);
            books.insert(
                component_id,
                NativePriceData {
                    base_address: base,
                    quote_address: quote,
                    minimum_in_base,
                    minimum_in_quote,
                    minimum_out_base,
                    minimum_out_quote,
                    bids,
                    asks,
                },
            );
        }

        books
    }

    /// The most liquid book that prices `quote_address` in a USD quote token, for normalizing the
    /// TVL of books quoted in some other token.
    fn select_tvl_conversion_book<'a>(
        &self,
        quote_address: &Bytes,
        books: &'a HashMap<String, NativePriceData>,
    ) -> Option<&'a NativePriceData> {
        books
            .values()
            .filter(|candidate| {
                // `group_orderbook` keeps the configured quote token on the quote side, so every
                // matching conversion book is valued in comparable approved-token units.
                candidate.base_address == *quote_address &&
                    self.usd_quote_tokens
                        .contains(&candidate.quote_address)
            })
            .filter_map(|candidate| {
                candidate
                    .calculate_tvl(None)
                    .map(|liquidity| (candidate, liquidity))
            })
            .max_by(|(candidate_a, liquidity_a), (candidate_b, liquidity_b)| {
                liquidity_a
                    .total_cmp(liquidity_b)
                    // Prefer the smaller token address when liquidity is equal so the result does
                    // not depend on HashMap or HashSet iteration order.
                    .then_with(|| {
                        candidate_b
                            .quote_address
                            .as_ref()
                            .cmp(candidate_a.quote_address.as_ref())
                    })
            })
            .map(|(candidate, _)| candidate)
    }

    /// The book's TVL in USD quote-token units. A book quoted in another token is normalized
    /// through the most liquid grouped book that prices its quote token in a USD quote token;
    /// `None` when there is none or the value is not finite.
    fn normalized_tvl(
        &self,
        book: &NativePriceData,
        grouped: &HashMap<String, NativePriceData>,
    ) -> Option<f64> {
        let conversion_book = if self
            .usd_quote_tokens
            .contains(&book.quote_address)
        {
            None
        } else {
            Some(self.select_tvl_conversion_book(&book.quote_address, grouped)?)
        };
        book.calculate_tvl(conversion_book)
    }

    /// Builds the simulation component and state for one grouped book. The state shares this
    /// source's client, so binding quotes carry the feed's full configuration.
    fn build_book(
        &self,
        component_id: &str,
        base_token: Token,
        quote_token: Token,
        book: NativePriceData,
    ) -> (ProtocolComponent, NativeState) {
        let state = NativeState {
            base_token: base_token.clone(),
            quote_token: quote_token.clone(),
            book,
            client: Arc::clone(&self.client),
        };
        let component = pair_component(
            component_id,
            PROTOCOL_SYSTEM,
            "native_relay_pool",
            self.common.chain,
            base_token,
            quote_token,
        );
        (component, state)
    }
}

impl HttpBookSource for NativeBookSource {
    /// Native Relay publishes its complete aggregated orderbook in one request; every poll
    /// rebuilds the whole set of books from it.
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
        let grouped = self.group_orderbook(self.fetch_orderbook().await?);
        // TVL normalization looks across all grouped books (unrequested ones included, as
        // conversion books), so it runs before the books are consumed.
        let tvls: HashMap<String, Option<f64>> = grouped
            .iter()
            .map(|(component_id, book)| (component_id.clone(), self.normalized_tvl(book, &grouped)))
            .collect();

        let mut books = HashMap::new();
        for (component_id, book) in grouped {
            // Only requested markets become components.
            let Some((base_token, quote_token)) = self
                .common
                .pair_tokens(&book.base_address, &book.quote_address)
            else {
                continue;
            };
            let Some(tvl) = tvls[&component_id] else {
                debug!(
                    %component_id,
                    "skipping pair, no book prices its quote token in a USD quote token"
                );
                continue;
            };
            if !self
                .common
                .clears_min_tvl(tvl, &component_id)
            {
                continue;
            }

            let (component, state) =
                self.build_book(&component_id, base_token.clone(), quote_token.clone(), book);
            // Native's orderbook carries no per-book timestamp.
            books
                .insert(component_id, Book { component, state: Arc::new(state), updated_at: None });
        }
        Ok(books)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Mutex};

    use rstest::rstest;
    use tokio::time::{timeout, Duration};
    use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim};

    use super::*;
    use crate::book::{
        http::test_support::spawn_http_server, levels::PriceLevel, models::test_token_map,
    };

    fn test_source(
        endpoint: String,
        tokens: &[&Bytes],
        usd_quote_tokens: HashSet<Bytes>,
        min_tvl_usd: f64,
    ) -> NativeBookSource {
        let entries: Vec<_> = tokens
            .iter()
            .map(|address| (*address, "T", 18))
            .collect();
        NativeBookSource {
            common: CommonConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&entries)),
                min_tvl_usd,
            },
            chain: NativeSupportedChain::Ethereum,
            usd_quote_tokens: Arc::new(usd_quote_tokens),
            orderbook_endpoint: format!("{endpoint}/orderbook"),
            client: Arc::new(NativeClient::new(
                NativeSupportedChain::Ethereum,
                endpoint,
                "test-api-key".to_string(),
                Duration::from_secs(5),
            )),
            http: Client::new(),
        }
    }

    fn conversion_book(
        base_address: Bytes,
        quote_address: Bytes,
        quantity: f64,
        price: f64,
    ) -> NativePriceData {
        NativePriceData {
            base_address,
            quote_address,
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: Levels::new(vec![PriceLevel { quantity, price }]).unwrap(),
            asks: Levels::default(),
        }
    }

    #[test]
    fn selects_most_liquid_tvl_conversion_book() {
        let weth = Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap();
        let usdc = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let usdt = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let wbtc = Bytes::from_str("0x4444444444444444444444444444444444444444").unwrap();
        let unapproved = Bytes::from_str("0x5555555555555555555555555555555555555555").unwrap();
        let source = test_source(
            "http://native.example".to_string(),
            &[&weth, &usdc, &usdt, &wbtc, &unapproved],
            HashSet::from([usdc.clone(), usdt.clone()]),
            0.0,
        );
        let books = HashMap::from([
            ("usdc".to_string(), conversion_book(weth.clone(), usdc.clone(), 1.0, 100.0)),
            ("usdt".to_string(), conversion_book(weth.clone(), usdt.clone(), 2.0, 100.0)),
            ("unrelated".to_string(), conversion_book(wbtc, usdc, 1_000.0, 100.0)),
            ("unapproved".to_string(), conversion_book(weth.clone(), unapproved, 2_000.0, 100.0)),
        ]);

        let selected = source
            .select_tvl_conversion_book(&weth, &books)
            .expect("one conversion book");

        assert_eq!(selected.quote_address, usdt);
    }

    #[test]
    fn selects_lower_quote_address_for_equal_tvl_conversion_liquidity() {
        let weth = Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap();
        let lower_quote = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let higher_quote = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let source = test_source(
            "http://native.example".to_string(),
            &[&weth, &lower_quote, &higher_quote],
            HashSet::from([lower_quote.clone(), higher_quote.clone()]),
            0.0,
        );
        let books = HashMap::from([
            ("higher".to_string(), conversion_book(weth.clone(), higher_quote, 2.0, 100.0)),
            ("lower".to_string(), conversion_book(weth.clone(), lower_quote.clone(), 1.0, 200.0)),
        ]);

        let selected = source
            .select_tvl_conversion_book(&weth, &books)
            .expect("one conversion book");

        assert_eq!(selected.quote_address, lower_quote);
    }

    #[test]
    fn builds_component_and_state_from_relay_orderbook() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdt = Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap();
        let source = test_source(
            "http://native.example".to_string(),
            &[&weth, &usdt],
            HashSet::from([usdt.clone()]),
            0.0,
        );

        let books = source.group_orderbook(vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 0.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 0.0001, price: 3213.12345 }])
                    .unwrap(),
            },
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 0.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel { quantity: 2.0, price: 3214.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdt.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 6428.0, price: 1.0 / 3214.0 }])
                    .unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdt.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel {
                    quantity: 0.321312345,
                    price: 1.0 / 3213.12345,
                }])
                .unwrap(),
            },
        ]);

        let (component_id, book) = books
            .into_iter()
            .next()
            .expect("one grouped book");
        let base_token = source.common.tokens[&weth].clone();
        let quote_token = source.common.tokens[&usdt].clone();
        let (component, state) =
            source.build_book(&component_id, base_token.clone(), quote_token.clone(), book.clone());

        assert_eq!(component.id, Bytes::from(component_id.as_str()));
        assert_eq!(component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(component.protocol_type_name, "native_relay_pool");
        assert_eq!(component.tokens, vec![base_token.clone(), quote_token.clone()]);
        assert!(component.contract_ids.is_empty());
        assert_eq!(
            book.bids,
            Levels::new(vec![PriceLevel { quantity: 0.0001, price: 3213.12345 }]).unwrap()
        );
        assert_eq!(book.asks.len(), 1);
        let expected =
            NativeState { base_token, quote_token, book, client: Arc::clone(&source.client) };
        assert!(state.eq(&expected), "state should carry the grouped book");
    }

    #[test]
    fn uses_stable_component_id_and_direction_when_merging_mirrored_books() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let source = test_source(
            "http://native.example".to_string(),
            &[&weth, &usdc],
            HashSet::from([usdc.clone()]),
            0.0,
        );
        let entries = vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdc.clone(),
                minimum_in_base: 100_000_000_000.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdc.clone(),
                minimum_in_base: 300_000_000_000.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_100.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdc.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 2_000.0, price: 0.0005 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdc.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 250.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 2_000.0, price: 0.0005 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdc.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 400.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel { quantity: 2.0, price: 0.5 }]).unwrap(),
            },
        ];

        let forward_only = source.group_orderbook(entries[..2].to_vec());
        let reverse_entries = entries[2..].to_vec();
        let reverse_only = source.group_orderbook(reverse_entries.clone());
        let reversed_reverse_only = source.group_orderbook(
            reverse_entries
                .into_iter()
                .rev()
                .collect(),
        );
        assert_eq!(reverse_only, reversed_reverse_only);
        let component_id = unordered_pair_component_id("native", &usdc, &weth);

        let forward_book = forward_only
            .get(&component_id)
            .expect("forward-only book uses the stable component ID");
        assert_eq!(forward_book.base_address, weth);
        assert_eq!(forward_book.quote_address, usdc);
        assert_eq!(forward_book.minimum_in_base, 100_000_000_000.0);
        assert_eq!(forward_book.minimum_in_quote, 0.0);
        assert_eq!(forward_book.minimum_out_base, 300_000_000_000.0);
        assert_eq!(forward_book.minimum_out_quote, 0.0);

        let reverse_book = reverse_only
            .get(&component_id)
            .expect("reverse-only book uses the stable component ID");
        assert_eq!(reverse_book.base_address, weth);
        assert_eq!(reverse_book.quote_address, usdc);
        assert_eq!(reverse_book.minimum_in_base, 0.0);
        assert_eq!(reverse_book.minimum_in_quote, 250.0);
        assert_eq!(reverse_book.minimum_out_base, 0.0);
        assert_eq!(reverse_book.minimum_out_quote, 400.0);
        assert_eq!(
            reverse_book.bids,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2.0 }]).unwrap()
        );
        assert_eq!(
            reverse_book.asks,
            Levels::new(vec![
                PriceLevel { quantity: 1.0, price: 2_000.0 },
                PriceLevel { quantity: 1.0, price: 2_000.0 },
            ])
            .unwrap()
        );

        let mixed = source.group_orderbook(vec![entries[0].clone(), entries[2].clone()]);
        let mixed_book = mixed.get(&component_id).unwrap();
        assert_eq!(mixed_book.minimum_in_base, 100_000_000_000.0);
        assert_eq!(mixed_book.minimum_in_quote, 100.0);
        assert_eq!(mixed_book.minimum_out_base, 0.0);
        assert_eq!(mixed_book.minimum_out_quote, 0.0);
        assert_eq!(
            mixed_book.bids,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );
        assert_eq!(
            mixed_book.asks,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );

        let books = source.group_orderbook(entries.clone());
        let reversed_books = source.group_orderbook(entries.into_iter().rev().collect());

        assert_eq!(books, reversed_books);
        assert_eq!(books.len(), 1);
        let book = books.get(&component_id).unwrap();
        assert_eq!(book.base_address, weth);
        assert_eq!(book.quote_address, usdc);
        assert_eq!(book.minimum_in_base, 100_000_000_000.0);
        assert_eq!(book.minimum_in_quote, 0.0);
        assert_eq!(book.minimum_out_base, 300_000_000_000.0);
        assert_eq!(book.minimum_out_quote, 0.0);
        assert_eq!(
            book.bids,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );
        assert_eq!(
            book.asks,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_100.0 }]).unwrap()
        );
    }

    #[test]
    fn zero_only_direct_side_does_not_suppress_mirrored_liquidity() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let source = test_source(
            "http://native.example".to_string(),
            &[&weth, &usdc],
            HashSet::from([usdc.clone()]),
            0.0,
        );

        let books = source.group_orderbook(vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdc.clone(),
                minimum_in_base: 999.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 0.0, price: 2_000.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdc,
                quote_address: weth,
                minimum_in_base: 250.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel { quantity: 2.0, price: 0.5 }]).unwrap(),
            },
        ]);

        let book = books
            .values()
            .next()
            .expect("one grouped book");
        assert_eq!(book.bids, Levels::new(vec![PriceLevel { quantity: 1.0, price: 2.0 }]).unwrap());
        assert_eq!(book.minimum_in_base, 0.0);
        assert_eq!(book.minimum_out_quote, 250.0);
    }

    #[tokio::test]
    async fn requests_native_token_orderbooks() {
        let seen_target = Arc::new(Mutex::new(None));
        let record = Arc::clone(&seen_target);
        let server = spawn_http_server(move |target| {
            *record.lock().unwrap() = Some(target.to_string());
            Some(("200 OK", "[]".to_string()))
        })
        .await;
        let source = test_source(server.url(), &[], HashSet::new(), 0.0);

        let orderbook = source.fetch_orderbook().await.unwrap();

        assert!(orderbook.is_empty());
        let target = seen_target
            .lock()
            .unwrap()
            .clone()
            .expect("one request");
        assert!(target.starts_with("/orderbook?"), "{target}");
        assert!(target.contains("chain=ethereum"), "{target}");
        assert!(target.contains("showNative=0x0"), "{target}");
    }

    /// Native answers API errors with HTTP 200 and an error envelope; an invalid key is fatal,
    /// every other code is a connection error the next poll may recover from.
    #[rstest]
    #[case::unavailable_token_is_a_connection_error(171015, "quoted token not available", false)]
    #[case::invalid_key_is_fatal(201001, "auth get api key is invalid", true)]
    #[tokio::test]
    async fn classifies_orderbook_api_errors_sent_with_http_200(
        #[case] code: u32,
        #[case] message: &str,
        #[case] expect_fatal: bool,
    ) {
        let body = format!(r#"{{"code":{code},"message":"{message}"}}"#);
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let source = test_source(server.url(), &[], HashSet::new(), 0.0);

        let result = source.fetch_orderbook().await;

        let code = code.to_string();
        match result {
            Err(FeedError::FatalError(msg)) if expect_fatal => assert!(msg.contains(&code)),
            Err(FeedError::ConnectionError(msg)) if !expect_fatal => assert!(msg.contains(&code)),
            other => panic!("unexpected result for code {code}: {other:?}"),
        }
        assert_eq!(server.request_count(), 1);
    }

    /// A gateway or load-balancer failure arrives as a non-success status with a plain body
    /// instead of Native's error envelope.
    #[tokio::test]
    async fn non_success_status_without_error_envelope_is_a_connection_error() {
        let server =
            spawn_http_server(|_| Some(("502 Bad Gateway", "bad gateway".to_string()))).await;
        let source = test_source(server.url(), &[], HashSet::new(), 0.0);

        let result = source.fetch_orderbook().await;

        match result {
            Err(FeedError::ConnectionError(msg)) => {
                assert!(msg.contains("502"), "status missing from: {msg}");
                assert!(msg.contains("bad gateway"), "body missing from: {msg}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[rstest]
    #[case::with_conversion_helper(true, 300.0, true)]
    #[case::without_conversion_helper(false, 300.0, false)]
    #[case::below_normalized_tvl_threshold(true, 401.0, false)]
    #[tokio::test]
    async fn uses_unrequested_books_only_for_tvl_conversion(
        #[case] include_helper: bool,
        #[case] tvl_threshold: f64,
        #[case] expect_market: bool,
    ) {
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let usdt = Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap();
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let mut entries = vec![serde_json::json!({
            "base_address": weth.to_string(),
            "quote_address": usdt.to_string(),
            "minimum_in_base": 0.0,
            "side": "bid",
            "levels": [[2.0, 100.0]]
        })];
        if include_helper {
            // A reversed helper with a non-unit price distinguishes the market's 200 USDT of
            // liquidity from its normalized value of 400 USDC.
            entries.push(serde_json::json!({
                "base_address": usdc.to_string(),
                "quote_address": usdt.to_string(),
                "minimum_in_base": 0.0,
                "side": "bid",
                "levels": [[1_000.0, 0.5]]
            }));
        }
        let body = serde_json::to_string(&entries).unwrap();
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        // USDC is a USD quote token but not a requested token: its helper book must never be
        // emitted.
        let source =
            test_source(server.url(), &[&weth, &usdt], HashSet::from([usdc]), tvl_threshold);

        let books = timeout(Duration::from_secs(5), source.fetch_books())
            .await
            .expect("orderbook poll timed out")
            .expect("orderbook poll failed");

        if expect_market {
            assert_eq!(books.len(), 1, "helper must not be emitted");
            let book = books.values().next().unwrap();
            assert_eq!(
                book.component
                    .tokens
                    .iter()
                    .map(|token| token.address.clone())
                    .collect::<Vec<_>>(),
                vec![weth, usdt]
            );
            assert_eq!(book.updated_at, None);
        } else {
            assert!(books.is_empty());
        }
    }
}
