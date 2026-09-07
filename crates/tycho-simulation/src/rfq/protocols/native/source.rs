use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tracing::{debug, warn};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::{pair_component, pair_component_id},
        Book, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    protocol::models::ProtocolComponent,
    rfq::protocols::native::{
        client::NativeClient,
        models::{NativeBookSide, NativeOrderbookEntry, NativeOrderbookSide, NativePriceData},
        state::NativeState,
        PROTOCOL_SYSTEM,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Native Relay book per poll: the aggregated orderbook, grouped into one
/// two-sided book per pair, filtered to the configured tokens and the USD TVL threshold.
#[derive(Clone, Debug)]
pub struct NativeBookSource {
    pub common: BookFeedConfig,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// Requests binding quotes; shared with every state this source emits. Its key also
    /// authenticates the orderbook poll.
    pub client: Arc<NativeClient>,
}

impl NativeBookSource {
    /// The most liquid book that prices `quote_address` in a USD quote token, for normalizing the
    /// TVL of books quoted in some other token. A book is oriented by address order, so the USD
    /// token sits on either side; `get_mid_price` prices `quote_address` from the side it is on.
    fn select_tvl_conversion_book<'a>(
        &self,
        quote_address: &Bytes,
        books: &'a HashMap<Bytes, NativePriceData>,
    ) -> Option<&'a NativePriceData> {
        books
            .values()
            .filter_map(|candidate| {
                let usd_side = if candidate.base_address == *quote_address {
                    &candidate.quote_address
                } else if candidate.quote_address == *quote_address {
                    &candidate.base_address
                } else {
                    return None;
                };
                if !self.usd_quote_tokens.contains(usd_side) {
                    return None;
                }
                let liquidity = usd_liquidity(candidate, usd_side)?;
                Some((candidate, usd_side, liquidity))
            })
            .max_by(|(_, usd_a, liquidity_a), (_, usd_b, liquidity_b)| {
                liquidity_a
                    .total_cmp(liquidity_b)
                    // Prefer the smaller token address when liquidity is equal so the result does
                    // not depend on HashMap or HashSet iteration order.
                    .then_with(|| usd_b.cmp(usd_a))
            })
            .map(|(candidate, _, _)| candidate)
    }

    /// The book's TVL in USD quote-token units. A book quoted in another token is normalized
    /// through the most liquid grouped book that prices its quote token in a USD quote token;
    /// `None` when there is none or the value is not finite.
    fn normalized_tvl(
        &self,
        book: &NativePriceData,
        grouped: &HashMap<Bytes, NativePriceData>,
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
        component_id: Bytes,
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

impl NativeBookSource {
    /// Native Relay publishes its complete aggregated orderbook in one request; every poll
    /// rebuilds the whole set of books from it.
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
        let grouped = group_orderbook(self.client.fetch_orderbook().await?);
        // TVL normalization looks across all grouped books (unrequested ones included, as
        // conversion books), so it runs before the books are consumed.
        let tvls: HashMap<Bytes, Option<f64>> = grouped
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

            let book_key = component_id.to_string();
            let (component, state) =
                self.build_book(component_id, base_token.clone(), quote_token.clone(), book);
            // Native's orderbook carries no per-book timestamp.
            books.insert(book_key, Book { component, state: Arc::new(state), updated_at: None });
        }
        Ok(books)
    }
}

/// Merges the directional orderbook entries into one two-sided book per pair, keyed by a
/// component id derived from the address-sorted pair so it stays stable whichever orientation
/// Native publishes in a given poll.
///
/// Native publishes each pair, side and orientation once per poll, aggregated across its
/// makers (verified against all four supported chains), so each entry fills one side of one
/// book, and a second entry for a side is dropped with a warning.
fn group_orderbook(mut entries: Vec<NativeOrderbookEntry>) -> HashMap<Bytes, NativePriceData> {
    // The book's own orientation first, so a side comes from the mirrored entry only where the
    // pair does not publish that side directly: inverting a ladder drops the levels whose
    // inverse overflows, and states the minimum in the other token.
    entries.sort_by_key(|entry| entry.base_address > entry.quote_address);

    let mut books: HashMap<Bytes, NativePriceData> = HashMap::new();
    for entry in entries {
        let mirrored = entry.base_address > entry.quote_address;
        let levels = if mirrored { entry.levels.invert() } else { entry.levels };

        // A side left empty by placeholder levels, or by inverses that all overflow, must not
        // claim its slot: the entry published the other way round may still fill it.
        if levels.is_empty() {
            continue;
        }

        // The book's base and quote, which are the entry's the other way round when it is
        // mirrored — that is what being mirrored means.
        let (base, quote) = if mirrored {
            (entry.quote_address, entry.base_address)
        } else {
            (entry.base_address, entry.quote_address)
        };

        let book = books
            .entry(pair_component_id(PROTOCOL_SYSTEM, &base, &quote))
            // Empty until the entries fill it: each side's ladder and minimum come from the one
            // entry that claims it, and a side no entry claims stays empty.
            .or_insert_with(|| NativePriceData {
                base_address: base.clone(),
                quote_address: quote.clone(),
                bids: NativeBookSide::default(),
                asks: NativeBookSide::default(),
            });

        // Which side an entry prices, and which end of it the entry's minimum constrains. A bid
        // states its minimum on the amount it takes in, an ask on the amount it pays out, both
        // in the entry's own base token — which is the token that end moves, either way round.
        let (ladder, minimum, which) = match (mirrored, entry.side) {
            (false, NativeOrderbookSide::Bid) => {
                (&mut book.bids.levels, &mut book.bids.minimum_in, "bid")
            }
            (false, NativeOrderbookSide::Ask) => {
                (&mut book.asks.levels, &mut book.asks.minimum_out, "ask")
            }
            (true, NativeOrderbookSide::Bid) => {
                (&mut book.asks.levels, &mut book.asks.minimum_in, "mirrored bid")
            }
            (true, NativeOrderbookSide::Ask) => {
                (&mut book.bids.levels, &mut book.bids.minimum_out, "mirrored ask")
            }
        };

        if !ladder.is_empty() {
            warn!(%base, %quote, side = which, "ignoring a second Native entry for one side");
            continue;
        }

        *ladder = levels;
        *minimum = entry.minimum_in_base;
    }

    books
}

/// A book's own liquidity in `usd_token` units, which the book prices on one of its two sides.
/// Comparable across books quoted in different tokens, unlike the raw quote-unit notional.
fn usd_liquidity(book: &NativePriceData, usd_token: &Bytes) -> Option<f64> {
    let liquidity = book.calculate_tvl(None)?;
    if book.quote_address == *usd_token {
        return Some(liquidity);
    }
    let price_of_quote_token = book.get_mid_price(liquidity, &book.quote_address)?;
    let converted = liquidity * price_of_quote_token;
    converted
        .is_finite()
        .then_some(converted)
}

impl HttpSource for NativeBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        self.fetch_books()
            .await
            .map(BookSnapshot::received_now)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Mutex};

    use rstest::rstest;
    use tokio::time::{timeout, Duration};
    use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim};

    use super::*;
    use crate::{
        book::{
            levels::{Levels, PriceLevel},
            test_token_map,
        },
        rfq::protocols::native::models::NativeSupportedChain,
        snapshot_feed::http::test_support::spawn_http_server,
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
            common: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&entries)),
                min_tvl_usd,
            },
            usd_quote_tokens: Arc::new(usd_quote_tokens),
            client: Arc::new(NativeClient::new(
                NativeSupportedChain::Ethereum,
                endpoint,
                "test-api-key".to_string(),
                Duration::from_secs(5),
            )),
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
            bids: NativeBookSide {
                levels: Levels::new(vec![PriceLevel { quantity, price }]).unwrap(),
                minimum_in: 0.0,
                minimum_out: 0.0,
            },
            asks: NativeBookSide::default(),
        }
    }

    /// The grouped books as `select_tvl_conversion_book` sees them: keyed by component id.
    fn books_by_id(
        books: impl IntoIterator<Item = NativePriceData>,
    ) -> HashMap<Bytes, NativePriceData> {
        books
            .into_iter()
            .map(|book| {
                (pair_component_id(PROTOCOL_SYSTEM, &book.base_address, &book.quote_address), book)
            })
            .collect()
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
        let books = books_by_id([
            conversion_book(weth.clone(), usdc.clone(), 1.0, 100.0),
            conversion_book(weth.clone(), usdt.clone(), 2.0, 100.0),
            conversion_book(wbtc, usdc, 1_000.0, 100.0),
            conversion_book(weth.clone(), unapproved, 2_000.0, 100.0),
        ]);

        let selected = source
            .select_tvl_conversion_book(&weth, &books)
            .expect("one conversion book");

        assert_eq!(selected.quote_address, usdt);
    }

    #[test]
    fn compares_tvl_conversion_books_in_usd_whichever_side_holds_the_usd_token() {
        let weth = Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap();
        let usdc = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let source = test_source(
            "http://native.example".to_string(),
            &[&weth, &usdc],
            HashSet::from([usdc.clone()]),
            0.0,
        );
        let books = books_by_id([
            // 100 USDC of liquidity, USDC on the quote side.
            conversion_book(weth.clone(), usdc.clone(), 1.0, 100.0),
            // 1 WETH of liquidity at 2000 USDC per WETH, USDC on the base side.
            conversion_book(usdc.clone(), weth.clone(), 2_000.0, 0.0005),
        ]);

        let selected = source
            .select_tvl_conversion_book(&weth, &books)
            .expect("one conversion book");

        assert_eq!(selected.base_address, usdc);
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
        let books = books_by_id([
            conversion_book(weth.clone(), higher_quote, 2.0, 100.0),
            conversion_book(weth.clone(), lower_quote.clone(), 1.0, 200.0),
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

        let books = group_orderbook(vec![
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

        let (component, state) = source.build_book(
            component_id.clone(),
            base_token.clone(),
            quote_token.clone(),
            book.clone(),
        );

        assert_eq!(component.id, component_id);
        assert_eq!(component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(component.protocol_type_name, "native_relay_pool");
        assert_eq!(component.tokens, vec![base_token.clone(), quote_token.clone()]);
        assert!(component.contract_ids.is_empty());
        assert_eq!(
            book.bids.levels,
            Levels::new(vec![PriceLevel { quantity: 0.0001, price: 3213.12345 }]).unwrap()
        );
        assert_eq!(book.asks.levels.len(), 1);
        let expected =
            NativeState { base_token, quote_token, book, client: Arc::clone(&source.client) };
        assert!(state.eq(&expected), "state should carry the grouped book");
    }

    #[test]
    fn uses_stable_component_id_and_direction_when_merging_mirrored_books() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdt = Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();
        let entries = vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 100_000_000_000.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 300_000_000_000.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_100.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdt.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 100.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 2_000.0, price: 0.0005 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdt.clone(),
                quote_address: weth.clone(),
                minimum_in_base: 400.0,
                side: NativeOrderbookSide::Ask,
                levels: Levels::new(vec![PriceLevel { quantity: 2.0, price: 0.5 }]).unwrap(),
            },
        ];

        let forward_only = group_orderbook(entries[..2].to_vec());
        let reverse_entries = entries[2..].to_vec();
        let reverse_only = group_orderbook(reverse_entries.clone());
        let reversed_reverse_only = group_orderbook(
            reverse_entries
                .into_iter()
                .rev()
                .collect(),
        );
        assert_eq!(reverse_only, reversed_reverse_only);
        // WETH sorts below USDT, so that is the orientation every grouping of this pair takes.
        let component_id = pair_component_id(PROTOCOL_SYSTEM, &weth, &usdt);

        let forward_book = forward_only
            .get(&component_id)
            .expect("forward-only book uses the stable component ID");
        assert_eq!(forward_book.base_address, weth);
        assert_eq!(forward_book.quote_address, usdt);
        assert_eq!(forward_book.bids.minimum_in, 100_000_000_000.0);
        assert_eq!(forward_book.bids.minimum_out, 0.0);
        assert_eq!(forward_book.asks.minimum_in, 0.0);
        assert_eq!(forward_book.asks.minimum_out, 300_000_000_000.0);

        let reverse_book = reverse_only
            .get(&component_id)
            .expect("reverse-only book uses the stable component ID");
        assert_eq!(reverse_book.base_address, weth);
        assert_eq!(reverse_book.quote_address, usdt);
        assert_eq!(reverse_book.bids.minimum_in, 0.0);
        assert_eq!(reverse_book.bids.minimum_out, 400.0);
        assert_eq!(reverse_book.asks.minimum_in, 100.0);
        assert_eq!(reverse_book.asks.minimum_out, 0.0);

        assert_eq!(
            reverse_book.bids.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2.0 }]).unwrap()
        );
        assert_eq!(
            reverse_book.asks.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );

        let mixed = group_orderbook(vec![entries[0].clone(), entries[2].clone()]);
        let mixed_book = mixed.get(&component_id).unwrap();
        assert_eq!(mixed_book.bids.minimum_in, 100_000_000_000.0);
        assert_eq!(mixed_book.bids.minimum_out, 0.0);
        assert_eq!(mixed_book.asks.minimum_in, 100.0);
        assert_eq!(mixed_book.asks.minimum_out, 0.0);

        assert_eq!(
            mixed_book.bids.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );
        assert_eq!(
            mixed_book.asks.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );

        let books = group_orderbook(entries.clone());
        let reversed_books = group_orderbook(entries.into_iter().rev().collect());

        assert_eq!(books, reversed_books);
        assert_eq!(books.len(), 1);
        let book = books.get(&component_id).unwrap();
        assert_eq!(book.base_address, weth);
        assert_eq!(book.quote_address, usdt);
        assert_eq!(book.bids.minimum_in, 100_000_000_000.0);
        assert_eq!(book.asks.minimum_in, 0.0);
        assert_eq!(book.asks.minimum_out, 300_000_000_000.0);
        assert_eq!(book.bids.minimum_out, 0.0);
        assert_eq!(
            book.bids.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );
        assert_eq!(
            book.asks.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_100.0 }]).unwrap()
        );
    }

    /// Native's Ethereum orderbook as the API answered on 2026-09-17, entry for entry as it
    /// arrived; only the whitespace between entries was changed.
    const LIVE_ETHEREUM_ORDERBOOK: &str = include_str!("test_responses/orderbook_ethereum.json");

    #[test]
    fn groups_the_live_orderbook_into_one_book_per_pair() {
        let entries: Vec<NativeOrderbookEntry> =
            serde_json::from_str(LIVE_ETHEREUM_ORDERBOOK).unwrap();

        // Native aggregates its makers into one entry per directed pair, and publishes every one
        // of them as a bid: a pair's asks are the entry published the other way round, which is
        // why the mirrored half of the grouping carries most of the book rather than an edge case.
        assert_eq!(entries.len(), 53);
        assert!(entries
            .iter()
            .all(|entry| entry.side == NativeOrderbookSide::Bid));

        let books = group_orderbook(entries);

        // 29 pairs, of which 24 were published in both orientations and so end up two-sided.
        assert_eq!(books.len(), 29);
        assert_eq!(
            books
                .values()
                .filter(|book| !book.bids.levels.is_empty() && !book.asks.levels.is_empty())
                .count(),
            24
        );

        // USDC sorts below WETH, so the book is oriented USDC/WETH: its bids are the USDC-based
        // entry untouched, its asks the inverse of the WETH-based one. 21.1326 WETH at
        // 2433.905763742478 USDC each inverts to 51434.75694286429 USDC at 1/2433.905763742478 =
        // 0.00041086225066592433 WETH each — 6.2 bps above the best bid, the spread Native quotes
        // this pair at.
        let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let book = &books[&pair_component_id(PROTOCOL_SYSTEM, &usdc, &weth)];

        assert_eq!(book.bids.levels.len(), 8);
        assert_eq!(
            book.bids.levels[0],
            PriceLevel { quantity: 44456.136566078654, price: 0.0004106092299061457 }
        );
        assert_eq!(book.asks.levels.len(), 5);
        assert_eq!(
            book.asks.levels[0],
            PriceLevel { quantity: 51434.75694286429, price: 0.00041086225066592433 }
        );
        // The minimum travels with the entry it was published on: 100 atomic USDC to sell USDC
        // into the bids, 1e11 wei to sell WETH into the asks. Native publishes no ask entry, so
        // nothing constrains the output side.
        assert_eq!(book.bids.minimum_in, 100.0);
        assert_eq!(book.asks.minimum_in, 100_000_000_000.0);
        assert_eq!(book.asks.minimum_out, 0.0);
        assert_eq!(book.bids.minimum_out, 0.0);
    }

    #[test]
    fn a_second_entry_for_one_side_is_kept_out_of_the_ladder() {
        // Native's aggregated orderbook publishes each side once, so this arrives only if that
        // stops being true.
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdt = Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();
        let bid = |minimum: f64, price: f64| NativeOrderbookEntry {
            base_address: weth.clone(),
            quote_address: usdt.clone(),
            minimum_in_base: minimum,
            side: NativeOrderbookSide::Bid,
            levels: Levels::new(vec![PriceLevel { quantity: 1.0, price }]).unwrap(),
        };

        let books = group_orderbook(vec![bid(100.0, 2_000.0), bid(300.0, 1_900.0)]);

        let book = books
            .get(&pair_component_id(PROTOCOL_SYSTEM, &weth, &usdt))
            .unwrap();
        assert_eq!(
            book.bids.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2_000.0 }]).unwrap()
        );
        assert_eq!(book.bids.minimum_in, 100.0);
    }

    #[test]
    fn zero_only_direct_side_does_not_suppress_mirrored_liquidity() {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdt = Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();

        let books = group_orderbook(vec![
            NativeOrderbookEntry {
                base_address: weth.clone(),
                quote_address: usdt.clone(),
                minimum_in_base: 999.0,
                side: NativeOrderbookSide::Bid,
                levels: Levels::new(vec![PriceLevel { quantity: 0.0, price: 2_000.0 }]).unwrap(),
            },
            NativeOrderbookEntry {
                base_address: usdt,
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
        assert_eq!(
            book.bids.levels,
            Levels::new(vec![PriceLevel { quantity: 1.0, price: 2.0 }]).unwrap()
        );
        assert_eq!(book.bids.minimum_in, 0.0);
        assert_eq!(book.bids.minimum_out, 250.0);
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

        let orderbook = source
            .client
            .fetch_orderbook()
            .await
            .unwrap();

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

    /// Native answers API errors with HTTP 200 and an error envelope; every code is a connection
    /// error a later poll may recover from, a refused key included — it is fixed at the venue,
    /// not by restarting the feed. The messages are the ones the live API returned on 2026-09-17.
    #[rstest]
    #[case::unavailable_token(171015, "quoted token not available")]
    #[case::invalid_key(201001, "auth get api key is invalid")]
    #[case::unknown_key(201006, "api_key not found")]
    #[tokio::test]
    async fn classifies_orderbook_api_errors_sent_with_http_200(
        #[case] code: u32,
        #[case] message: &str,
    ) {
        let body = format!(r#"{{"code":{code},"message":"{message}"}}"#);
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let source = test_source(server.url(), &[], HashSet::new(), 0.0);

        let result = source.client.fetch_orderbook().await;

        let code = code.to_string();
        match result {
            Err(FeedError::Connection(msg)) => assert!(msg.contains(&code), "{msg}"),
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

        let result = source.client.fetch_orderbook().await;

        match result {
            Err(FeedError::Connection(msg)) => {
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
