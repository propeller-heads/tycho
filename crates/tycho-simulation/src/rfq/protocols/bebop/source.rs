use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use chrono::DateTime;
use http::Request;
use itertools::Itertools;
use prost::Message as ProstMessage;
use tracing::{debug, warn};
use tycho_common::Bytes;

use crate::{
    book::{
        all_pairs::{BookCollector, Venue, VenueBook},
        component::pair_component_id,
        Book, BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    rfq::protocols::bebop::{
        client::{BebopClient, BebopPricingClient},
        models::{BebopBook, BebopPrices, BebopPricingUpdate},
        state::{BebopAllPairsState, BebopState},
        PROTOCOL_TYPE,
    },
    snapshot_feed::{
        errors::FeedError,
        ws::{WsPayload, WsSource},
    },
};

/// Streams Bebop's pricing WebSocket and decodes every pricing frame into the complete book its
/// [`BebopBookAssembler`] makes of it.
#[derive(Clone, Debug)]
pub struct BebopBookSource {
    pub(super) prices: BebopPricesSource,
    pub(super) assembler: BebopBookAssembler,
}

impl WsSource for BebopBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    fn request(&self) -> Result<Request<()>, FeedError> {
        self.prices.request()
    }

    fn decode(
        &mut self,
        payload: WsPayload,
    ) -> Result<Option<BookSnapshot<ReceivedAt>>, FeedError> {
        Ok(self
            .prices
            .decode(payload)?
            .map(|prices| BookSnapshot::received_now(self.assembler.collect(prices))))
    }
}

/// Reads the pricing WebSocket through a [`BebopPricingClient`].
#[derive(Clone, Debug)]
pub(super) struct BebopPricesSource {
    client: BebopPricingClient,
}

impl BebopPricesSource {
    pub(super) fn new(client: BebopPricingClient) -> Self {
        Self { client }
    }
}

impl WsSource for BebopPricesSource {
    type Snapshot = BebopPrices;

    fn request(&self) -> Result<Request<()>, FeedError> {
        self.client
            .pricing_handshake()
            // No retry builds a different request, so the feed ends rather than reconnecting.
            .map_err(|e| FeedError::Fatal(format!("Failed to build the pricing handshake: {e}")))
    }

    fn decode(&mut self, payload: WsPayload) -> Result<Option<BebopPrices>, FeedError> {
        decode_frame(payload)
    }
}

/// Decodes one pricing frame. A text frame carries no prices and yields `None`.
fn decode_frame(payload: WsPayload) -> Result<Option<BebopPrices>, FeedError> {
    let data = match payload {
        WsPayload::Binary(data) => data,
        // The protobuf stream carries pricing only in binary frames; a text frame is most
        // likely a server-side notice worth seeing in the logs.
        WsPayload::Text(text) => {
            warn!(%text, "unexpected text frame on the pricing stream");
            return Ok(None);
        }
    };
    let update = BebopPricingUpdate::decode(&data[..])
        .map_err(|e| FeedError::Parsing(format!("Failed to parse protobuf message: {e}")))?;
    BebopPrices::from_update(update).map(Some)
}

/// Turns [`BebopPrices`] into the books [`BebopFeed`](super::feed::BebopFeed) would publish for
/// the same frame: each pair between the configured tokens whose normalized TVL clears the
/// threshold, as simulate-ready books whose states request firm quotes through the assembler's
/// client. Created by [`BebopBookAssemblerBuilder`](super::feed::BebopBookAssemblerBuilder), which
/// also makes the one a [`BebopFeed`](super::feed::BebopFeed) uses.
#[derive(Clone, Debug)]
pub struct BebopBookAssembler {
    /// The protocol system stamped on the components: the venue's own, or its fallback-router
    /// one for a feed built with `with_fallback_router()`.
    pub(super) protocol_system: &'static str,
    pub(super) book_config: BookFeedConfig,
    /// USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub(super) usd_quote_tokens: Arc<BTreeSet<Bytes>>,
    /// Requests binding quotes; shared with every state built here.
    pub(super) client: Arc<BebopClient>,
    /// Whether Bebop's pairs are published as one component or one each. Bebop names no
    /// market maker, so that component is quoted once per venue.
    pub(super) all_pairs: bool,
}

impl BebopBookAssembler {
    /// Returns the books for `prices`, keyed by component id. USD conversion uses pairs from the
    /// whole update, including pairs outside the configured tokens, to price each book's quote
    /// token.
    pub fn books(&self, prices: BebopPrices) -> HashMap<String, Book> {
        self.collect(prices).into_map()
    }

    pub(super) fn collect(&self, prices: BebopPrices) -> Books {
        let pairs = prices.pairs;
        // Measure all pairs before consuming any, because USD conversion can use other pairs in
        // the list. Each result contains the tokens, TVL and quote-token price needed to build a
        // book.
        let measured = pairs
            .iter()
            .map(|book| {
                let (base_token, quote_token) = self
                    .book_config
                    .pair_tokens(&book.base, &book.quote)?;
                let (tvl, usd_per_quote_token) = self.measure_in_usd(book, &pairs)?;
                Some((base_token, quote_token, tvl, usd_per_quote_token))
            })
            .collect_vec();

        let mut collector = BookCollector::new(
            self.protocol_system,
            PROTOCOL_TYPE,
            self.book_config.chain,
            self.all_pairs.then_some(()),
        );
        for (book, (base_token, quote_token, tvl, usd_per_quote_token)) in pairs
            .into_iter()
            .zip(measured)
            .filter_map(|(book, measured)| measured.map(|measured| (book, measured)))
        {
            let component_id = pair_component_id(self.protocol_system, &book.base, &book.quote);
            if !self
                .book_config
                .clears_min_tvl(tvl, &component_id)
            {
                continue;
            }
            // `last_update_ts` is milliseconds on the wire (Bebop's spec says seconds; 13-digit
            // values observed live).
            let updated_at = DateTime::from_timestamp_millis(book.last_update_ts as i64);
            let state = BebopState::new(
                base_token.clone(),
                quote_token.clone(),
                book,
                usd_per_quote_token,
                Arc::clone(&self.client),
            );
            collector.push(component_id, VenueBook { signer: Venue, updated_at, state });
        }
        collector.finish::<BebopAllPairsState>()
    }

    /// What the book is worth in USD and what one of its quote tokens is worth there: its TVL,
    /// and the price the venue's dollar floor is read against. Both rest on the same pair — the
    /// one pricing the book's quote token in a USD quote token, or none at all when the quote
    /// token is one itself — so a book that has one has both. `None` when nothing in the update
    /// prices that token, or when the pair that does is one-sided and names no price.
    ///
    /// The two are priced at different sizes on purpose: a book's TVL is worth what converting
    /// all of it would fetch, where a floor of one dollar is a trade at the top of the ladder.
    fn measure_in_usd(&self, book: &BebopBook, pairs: &[BebopBook]) -> Option<(f64, f64)> {
        if self
            .usd_quote_tokens
            .contains(&book.quote)
        {
            return Some((book.calculate_tvl(None), 1.0));
        }
        let Some(quote_book) = self.conversion_book(book, pairs) else {
            debug!(
                base = %book.base,
                quote = %book.quote,
                "skipping pair, no book prices its quote token in a USD quote token"
            );
            return None;
        };
        let usd_per_quote_token = quote_book.get_mid_price(1.0, &book.quote)?;
        Some((book.calculate_tvl(Some(quote_book)), usd_per_quote_token))
    }

    /// A pair in the same update that prices `book`'s quote token in a USD quote token, in
    /// either orientation. When several USD quote tokens price it, the one with the lowest
    /// address wins, so every process assembling the same prices picks the same pair.
    fn conversion_book<'a>(
        &self,
        book: &BebopBook,
        pairs: &'a [BebopBook],
    ) -> Option<&'a BebopBook> {
        self.usd_quote_tokens
            .iter()
            .find_map(|usd_quote_token| {
                pairs.iter().find(|pair| {
                    (pair.base == book.quote && pair.quote == *usd_quote_token) ||
                        (pair.quote == book.quote && pair.base == *usd_quote_token)
                })
            })
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
            assert_same_books, decode_swap_directions, test_token_map, SWAP_DIRECTIONS_ATTRIBUTE,
        },
        rfq::protocols::bebop::{
            client::QuoteOrigin,
            models::{BebopPriceData, BebopSupportedChain},
            FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM,
        },
    };

    const USDC_HEX: &str = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const USDT_HEX: &str = "dAC17F958D2ee523a2206206994597C13D831ec7";

    /// Parses the pairs in one pricing update and builds books from them.
    fn build_books(
        assembler: &BebopBookAssembler,
        update: BebopPricingUpdate,
    ) -> Result<Books, FeedError> {
        BebopPrices::from_update(update).map(|prices| assembler.collect(prices))
    }

    fn test_client() -> Arc<BebopClient> {
        Arc::new(BebopClient::new(
            BebopSupportedChain::Ethereum,
            "test_key".to_string(),
            Duration::from_secs(5),
            QuoteOrigin::default(),
        ))
    }

    /// A WETH/USDC assembler with USDC as the only USD quote token.
    fn test_assembler(tvl: f64) -> BebopBookAssembler {
        let weth: Bytes = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let usdc: Bytes = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let tokens = test_token_map(&[(&weth, "WETH", 18), (&usdc, "USDC", 6)]);
        BebopBookAssembler {
            all_pairs: false,
            protocol_system: PROTOCOL_SYSTEM,
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd: tvl,
            },
            usd_quote_tokens: Arc::new(BTreeSet::from([usdc])),
            client: test_client(),
        }
    }

    fn weth_usdc_price_data(bid_price: f32) -> BebopPriceData {
        BebopPriceData {
            base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            last_update_ts: 1752617378,
            bids: vec![bid_price, 0.325717f32],
            asks: vec![bid_price + 0.5f32, 0.325717f32],
        }
    }

    #[rstest::rstest]
    #[case::approved_quote_included(0.0, 3000.0, true)]
    #[case::below_tvl_threshold_excluded(1e12, 3000.0, false)]
    fn build_books_tvl_threshold(
        #[case] tvl_threshold: f64,
        #[case] bid_price: f32,
        #[case] expect_included: bool,
    ) {
        let assembler = test_assembler(tvl_threshold);

        let update = BebopPricingUpdate { pairs: vec![weth_usdc_price_data(bid_price)] };
        let books = build_books(&assembler, update).unwrap();

        assert_eq!(books.len(), usize::from(expect_included));
        if expect_included {
            let book = books.values().next().unwrap();
            assert_eq!(book.component.protocol_system, "rfq:bebop");
        }
    }

    #[rstest]
    #[case::per_pair(
        false,
        // "fallback:rfq:bebop" in ASCII, then WETH and USDC.
        "0x66616c6c6261636b3a7266713a6265626f70\
           c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2\
           a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
    )]
    #[case::all_pairs(true, "0x66616c6c6261636b3a7266713a6265626f70")]
    fn a_fallback_assembler_labels_its_components_for_the_fallback_router(
        #[case] all_pairs: bool,
        #[case] expected_id: &str,
    ) {
        let assembler = BebopBookAssembler {
            protocol_system: FALLBACK_PROTOCOL_SYSTEM,
            all_pairs,
            ..test_assembler(0.0)
        };
        let update = BebopPricingUpdate { pairs: vec![weth_usdc_price_data(3000.0)] };

        let books = build_books(&assembler, update).unwrap();

        let book = books.values().next().expect("one book");
        assert_eq!(book.component.protocol_system, "fallback:rfq:bebop");
        assert_eq!(book.component.id.to_string(), expected_id);
    }

    #[test]
    fn build_books_skips_unknown_tokens() {
        let assembler = test_assembler(0.0);

        let mut price_data = weth_usdc_price_data(3000.0);
        price_data.base = hex::decode("1111111111111111111111111111111111111111").unwrap();
        let update = BebopPricingUpdate { pairs: vec![price_data] };

        assert!(build_books(&assembler, update)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn build_books_emits_each_published_orientation_as_its_own_component() {
        // Bebop streams some pairs from both sides. Each orientation is its own component,
        // identified by the pair in the order it was published.
        let assembler = test_assembler(0.0);
        let weth_usdc = weth_usdc_price_data(3000.0);
        let usdc_weth = BebopPriceData {
            base: weth_usdc.quote.clone(),
            quote: weth_usdc.base.clone(),
            last_update_ts: 1752617378,
            bids: vec![1.0 / 3000.5, 1000.0],
            asks: vec![1.0 / 3000.0, 1000.0],
        };
        let weth = Bytes::from(weth_usdc.base.clone());
        let usdc = Bytes::from(weth_usdc.quote.clone());

        let update = BebopPricingUpdate { pairs: vec![usdc_weth, weth_usdc] };
        let books = build_books(&assembler, update).unwrap();

        assert_eq!(books.len(), 2);
        let forward = &books[&pair_component_id(PROTOCOL_SYSTEM, &weth, &usdc).to_string()];
        assert_eq!(forward.component.tokens[0].symbol, "WETH");
        assert_eq!(forward.component.tokens[1].symbol, "USDC");
        let reverse = &books[&pair_component_id(PROTOCOL_SYSTEM, &usdc, &weth).to_string()];
        assert_eq!(reverse.component.tokens[0].symbol, "USDC");
        assert_eq!(reverse.component.tokens[1].symbol, "WETH");
        for book in [forward, reverse] {
            assert!(
                !book
                    .component
                    .static_attributes
                    .contains_key(SWAP_DIRECTIONS_ATTRIBUTE),
                "both sides of the book have levels, so its two tokens are the directions"
            );
        }
    }

    #[test]
    fn build_books_normalizes_unapproved_quote() {
        // WETH/WBTC quotes in WBTC (not approved); a WBTC/USDC pair provides the one-hop
        // price for normalization.
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let tokens = test_token_map(&[(&weth, "WETH", 18), (&wbtc, "WBTC", 8)]);
        let assembler = BebopBookAssembler {
            all_pairs: false,
            protocol_system: PROTOCOL_SYSTEM,
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd: 0.0,
            },
            usd_quote_tokens: Arc::new(BTreeSet::from([usdc])),
            client: test_client(),
        };

        let weth_wbtc = BebopPriceData {
            base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            quote: hex::decode("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            last_update_ts: 1752617378,
            bids: vec![0.05f32, 1.0f32],
            asks: vec![0.051f32, 1.0f32],
        };
        let wbtc_usdc = BebopPriceData {
            base: hex::decode("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            last_update_ts: 1752617378,
            bids: vec![60000.0f32, 1.0f32],
            asks: vec![60050.0f32, 1.0f32],
        };

        // With the normalization pair present, WETH/WBTC is emitted.
        let update = BebopPricingUpdate { pairs: vec![weth_wbtc.clone(), wbtc_usdc] };
        let books = build_books(&assembler, update).unwrap();
        assert_eq!(books.len(), 1);

        // The same pair prices the venue's dollar floor: one WBTC is worth the mid of the
        // 60 000 bid and the 60 050 ask it is quoted at.
        let state = books
            .values()
            .next()
            .unwrap()
            .state
            .as_any()
            .downcast_ref::<BebopState>()
            .expect("a Bebop state");
        assert_eq!(state.usd_per_quote_token, 60_025.0);

        // Without it, the unapproved quote cannot be priced and the pair is skipped.
        let update = BebopPricingUpdate { pairs: vec![weth_wbtc] };
        assert!(build_books(&assembler, update)
            .unwrap()
            .is_empty());
    }

    /// Published as one component, Bebop's pairs are one book advertising both directions of
    /// each of them, and the venue signs one quote for the route.
    #[test]
    fn build_books_publishes_every_pair_under_one_component() {
        let assembler = BebopBookAssembler { all_pairs: true, ..test_assembler(0.0) };
        let update = BebopPricingUpdate { pairs: vec![weth_usdc_price_data(3000.0)] };

        let books = build_books(&assembler, update).unwrap();

        assert_eq!(books.len(), 1);
        let book = books.values().next().unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        // The venue's protocol system in ASCII, which is the whole id.
        assert_eq!(book.component.id.to_string(), "0x7266713a6265626f70");
        assert_eq!(
            decode_swap_directions(&book.component.static_attributes["swap_directions"]).unwrap(),
            vec![(usdc.clone(), weth.clone()), (weth.clone(), usdc.clone())]
        );

        let tokens = &assembler.book_config.tokens;
        // A tenth of a WETH, well inside the published ladder.
        let swap = book
            .state
            .get_amount_out(
                num_bigint::BigUint::from(10u64).pow(17),
                &tokens[&weth],
                &tokens[&usdc],
            )
            .unwrap();
        let second = swap.new_state.get_amount_out(
            num_bigint::BigUint::from(10u64).pow(17),
            &tokens[&weth],
            &tokens[&usdc],
        );
        assert!(
            matches!(
                second,
                Err(tycho_common::simulation::errors::SimulationError::RecoverableError(_))
            ),
            "Bebop signs once per route, got {second:?}"
        );
    }

    /// A token priced against both USDC and USDT converts through the USD quote token with the
    /// lowest address, whatever order the set was filled in.
    #[rstest]
    #[case::usdc_first([USDC_HEX, USDT_HEX])]
    #[case::usdt_first([USDT_HEX, USDC_HEX])]
    fn the_usd_conversion_pair_does_not_depend_on_the_order_of_the_quote_tokens(
        #[case] quote_tokens: [&str; 2],
    ) {
        const OTHER_HEX: &str = "1234567890123456789012345678901234567890";
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let other = Bytes::from_str(OTHER_HEX).unwrap();
        let usd_quote_tokens: BTreeSet<Bytes> = quote_tokens
            .iter()
            .map(|token| Bytes::from_str(token).unwrap())
            .collect();
        let assembler = BebopBookAssembler {
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&other, "OTHER", 18)])),
                min_tvl_usd: 0.0,
            },
            usd_quote_tokens: Arc::new(usd_quote_tokens),
            ..test_assembler(0.0)
        };
        let pair = |base: &str, quote: &str, price: f32| BebopPriceData {
            base: hex::decode(base).unwrap(),
            quote: hex::decode(quote).unwrap(),
            last_update_ts: 1752617378,
            bids: vec![price, 1000.0],
            asks: vec![price, 1000.0],
        };
        let update = BebopPricingUpdate {
            pairs: vec![
                pair("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", OTHER_HEX, 100.0),
                pair(OTHER_HEX, USDT_HEX, 20.0),
                pair(OTHER_HEX, USDC_HEX, 10.0),
            ],
        };

        let books = build_books(&assembler, update).unwrap();

        let state = books
            .values()
            .next()
            .unwrap()
            .state
            .as_any()
            .downcast_ref::<BebopState>()
            .expect("a Bebop state");
        assert_eq!(state.usd_per_quote_token, 10.0);
    }

    fn binary_frame(update: &BebopPricingUpdate) -> WsPayload {
        WsPayload::Binary(update.encode_to_vec().into())
    }

    fn invalid_price_data() -> BebopPriceData {
        BebopPriceData { bids: vec![0.0f32, 1.0f32], ..weth_usdc_price_data(3000.0) }
    }

    /// Books rebuilt from serialized prices match the source's published books for the same
    /// frame.
    #[test]
    fn prices_round_trip_builds_the_published_books() {
        let assembler = test_assembler(0.0);
        let update = BebopPricingUpdate { pairs: vec![weth_usdc_price_data(3000.1)] };
        let mut source = BebopBookSource {
            prices: BebopPricesSource::new(BebopPricingClient::new(
                BebopSupportedChain::Ethereum,
                "test_key".to_string(),
            )),
            assembler: assembler.clone(),
        };
        let published = source
            .decode(binary_frame(&update))
            .unwrap()
            .unwrap();

        let prices = decode_frame(binary_frame(&update))
            .unwrap()
            .unwrap();
        let sent = serde_json::to_string(&prices).unwrap();
        let received: BebopPrices = serde_json::from_str(&sent).unwrap();

        assert_eq!(received, prices);
        assert_eq!(published.books.len(), 1);
        assert_same_books(&assembler.books(received), &published.books);
    }

    #[test]
    fn a_frame_decodes_into_its_valid_pairs() {
        let update =
            BebopPricingUpdate { pairs: vec![invalid_price_data(), weth_usdc_price_data(3000.0)] };

        let prices = decode_frame(binary_frame(&update))
            .unwrap()
            .unwrap();

        assert_eq!(
            prices,
            BebopPrices { pairs: vec![BebopBook::try_from(weth_usdc_price_data(3000.0)).unwrap()] }
        );
    }

    #[test]
    fn a_frame_with_no_valid_pair_is_a_parsing_error() {
        let update = BebopPricingUpdate { pairs: vec![invalid_price_data()] };

        assert!(matches!(decode_frame(binary_frame(&update)), Err(FeedError::Parsing(_))));
    }

    #[test]
    fn a_text_frame_carries_no_prices() {
        let frame = WsPayload::Text("maintenance".into());

        assert!(decode_frame(frame).unwrap().is_none());
    }
}
