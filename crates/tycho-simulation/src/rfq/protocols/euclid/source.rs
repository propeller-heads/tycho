use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use chrono::DateTime;
use http::Request;
use tracing::{debug, warn};
use tycho_common::Bytes;

use crate::{
    book::{
        all_pairs::{BookCollector, Venue, VenueBook},
        component::pair_component_id,
        tvl::tvl_in_usd,
        Book, BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    rfq::protocols::euclid::{
        client::{EuclidClient, EuclidLevelsClient},
        models::{EuclidLevelsFrame, EuclidPrices, EuclidSupportedChain},
        state::{EuclidAllPairsState, EuclidState},
        PROTOCOL_SYSTEM, PROTOCOL_TYPE,
    },
    snapshot_feed::{
        errors::FeedError,
        ws::{WsPayload, WsSource},
    },
};

/// Streams Euclid's levels WebSocket and decodes every frame into the complete book its
/// [`EuclidBookAssembler`] makes of it.
#[derive(Clone, Debug)]
pub struct EuclidBookSource {
    pub(super) prices: EuclidPricesSource,
    pub(super) assembler: EuclidBookAssembler,
}

impl WsSource for EuclidBookSource {
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

/// Reads the levels WebSocket through an [`EuclidLevelsClient`].
#[derive(Clone, Debug)]
pub(super) struct EuclidPricesSource {
    client: EuclidLevelsClient,
}

impl EuclidPricesSource {
    pub(super) fn new(client: EuclidLevelsClient) -> Self {
        Self { client }
    }
}

impl WsSource for EuclidPricesSource {
    type Snapshot = EuclidPrices;

    fn request(&self) -> Result<Request<()>, FeedError> {
        self.client
            .levels_handshake()
            // No retry builds a different request, so the feed ends rather than reconnecting.
            .map_err(|e| FeedError::Fatal(format!("Failed to build the levels handshake: {e}")))
    }

    fn decode(&mut self, payload: WsPayload) -> Result<Option<EuclidPrices>, FeedError> {
        decode_frame(payload, self.client.chain().id())
    }
}

/// Decodes one levels frame of the stream opened for `chain_id`. A binary frame carries no
/// levels and yields `None`; a frame for another chain is an error, since the stream was asked
/// for this one.
fn decode_frame(payload: WsPayload, chain_id: u64) -> Result<Option<EuclidPrices>, FeedError> {
    let text = match payload {
        WsPayload::Text(text) => text,
        // The levels stream is JSON text; a binary frame carries no levels.
        WsPayload::Binary(_) => {
            warn!("unexpected binary frame on the levels stream");
            return Ok(None);
        }
    };
    let frame: EuclidLevelsFrame = serde_json::from_str(text.as_str())
        .map_err(|e| FeedError::Parsing(format!("Failed to parse Euclid levels frame: {e}")))?;
    if frame.chain_id != chain_id {
        return Err(FeedError::Parsing(format!(
            "Euclid levels frame is for chain {}, the stream was opened for chain {chain_id}",
            frame.chain_id
        )));
    }
    EuclidPrices::from_frame(frame).map(Some)
}

/// Turns [`EuclidPrices`] into the books [`EuclidFeed`](super::feed::EuclidFeed) would publish
/// for the same frame: each pair between the configured tokens whose TVL, valued in a USD quote
/// token, clears the threshold, as simulate-ready books whose states request binding quotes
/// through the assembler's client. Created by
/// [`EuclidBookAssemblerBuilder`](super::feed::EuclidBookAssemblerBuilder), which also makes the
/// one an [`EuclidFeed`](super::feed::EuclidFeed) uses.
#[derive(Clone, Debug)]
pub struct EuclidBookAssembler {
    /// The chain the books are for; prices for any other chain assemble into none.
    pub(super) chain: EuclidSupportedChain,
    pub(super) book_config: BookFeedConfig,
    /// USD-priced tokens a book's quote token is valued in before the threshold applies.
    pub(super) usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// Requests binding quotes; shared with every state built here.
    pub(super) client: Arc<EuclidClient>,
    /// Whether Euclid's pairs are published as one component or one each. Euclid signs every
    /// quote from one maker inventory, so that component is quoted once per venue.
    pub(super) all_pairs: bool,
}

impl EuclidBookAssembler {
    /// Returns the books for `prices`, keyed by component id. Valuing a book's quote token in
    /// USD uses pairs from the whole frame, including pairs outside the configured tokens.
    pub fn books(&self, prices: EuclidPrices) -> HashMap<String, Book> {
        self.collect(prices).into_map()
    }

    pub(super) fn collect(&self, prices: EuclidPrices) -> Books {
        if prices.chain_id != self.chain.id() {
            warn!(
                frame_chain_id = prices.chain_id,
                chain_id = self.chain.id(),
                "prices are for another chain, assembling no books from them"
            );
            return Books::new();
        }
        let books = prices.pairs;

        // What one quote token of each book is worth in USD. The pair that prices a book's quote
        // token is another book in the same frame, so every book is valued before any of them is
        // consumed.
        let usd_per_quote_token: Vec<Option<f64>> = books
            .iter()
            .map(|book| {
                tvl_in_usd(
                    1.0,
                    &book.quote,
                    &self.usd_quote_tokens,
                    books
                        .iter()
                        .map(|other| (&other.base, &other.quote, &other.bids)),
                )
            })
            .collect();

        let mut collector = BookCollector::new(
            PROTOCOL_SYSTEM,
            PROTOCOL_TYPE,
            self.book_config.chain,
            self.all_pairs.then_some(()),
        );
        for (book, usd_per_quote_token) in books
            .into_iter()
            .zip(usd_per_quote_token)
        {
            let Some((base_token, quote_token)) = self
                .book_config
                .pair_tokens(&book.base, &book.quote)
            else {
                continue;
            };
            let component_id = pair_component_id(PROTOCOL_SYSTEM, &book.base, &book.quote);
            let Some(usd_per_quote_token) = usd_per_quote_token else {
                debug!(
                    %component_id,
                    "skipping pair, no book prices its quote token in a USD quote token"
                );
                continue;
            };
            if !self
                .book_config
                .clears_min_tvl(book.notional_tvl() * usd_per_quote_token, &component_id)
            {
                continue;
            }
            let updated_at = DateTime::from_timestamp_millis(book.updated_at_ms as i64);
            let state = EuclidState::new(
                base_token.clone(),
                quote_token.clone(),
                book,
                usd_per_quote_token,
                Arc::clone(&self.client),
            );
            collector.push(component_id, VenueBook { signer: Venue, updated_at, state });
        }
        collector.finish::<EuclidAllPairsState>()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        book::{all_pairs::QuoteRule, assert_same_books, test_token_map},
        rfq::protocols::euclid::client::EUCLID_API_URL,
    };

    /// A levels frame recorded from Euclid's gateway on Ethereum: WETH/USDC.
    const LEVELS_FRAME: &str = include_str!("test_responses/levels_frame.json");

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";

    fn bytes(value: &str) -> Bytes {
        Bytes::from_str(value).unwrap()
    }

    /// A WETH/WBTC/USDC assembler with USDC as the only USD quote token.
    fn test_assembler(min_tvl_usd: f64, all_pairs: bool) -> EuclidBookAssembler {
        let (weth, usdc, wbtc) = (bytes(WETH), bytes(USDC), bytes(WBTC));
        let tokens = test_token_map(&[(&weth, "WETH", 18), (&usdc, "USDC", 6), (&wbtc, "WBTC", 8)]);
        EuclidBookAssembler {
            chain: EuclidSupportedChain::Ethereum,
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd,
            },
            usd_quote_tokens: Arc::new(HashSet::from([usdc])),
            client: Arc::new(EuclidClient::new(
                EuclidSupportedChain::Ethereum,
                EUCLID_API_URL,
                None,
                Duration::from_secs(5),
            )),
            all_pairs,
        }
    }

    fn test_source(min_tvl_usd: f64, all_pairs: bool) -> EuclidBookSource {
        EuclidBookSource {
            prices: EuclidPricesSource::new(EuclidLevelsClient::new(
                EuclidSupportedChain::Ethereum,
                EUCLID_API_URL,
            )),
            assembler: test_assembler(min_tvl_usd, all_pairs),
        }
    }

    fn pair(base: &str, quote: &str, bid: &str, ask: &str, size: &str) -> String {
        format!(
            r#"{{"base_address":"{base}","quote_address":"{quote}","bids":[["{bid}","{size}"]],"asks":[["{ask}","{size}"]],"timestamp":1757500000000}}"#
        )
    }

    fn text_frame(chain_id: u64, pairs: &str) -> WsPayload {
        WsPayload::Text(format!(r#"{{"chain_id":{chain_id},"pairs":[{pairs}]}}"#).into())
    }

    /// The prices of a frame for chain 1 carrying `pairs`.
    fn prices(pairs: &str) -> EuclidPrices {
        decode_frame(text_frame(1, pairs), 1)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_recorded_frame_becomes_one_book_per_pair() {
        let mut source = test_source(0.0, false);
        let snapshot = source
            .decode(WsPayload::Text(LEVELS_FRAME.into()))
            .unwrap()
            .unwrap();

        let id = pair_component_id(PROTOCOL_SYSTEM, &bytes(WETH), &bytes(USDC));
        let book = &snapshot.books[&id.to_string()];
        assert_eq!(book.component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(book.component.protocol_type_name, PROTOCOL_TYPE);
        assert_eq!(
            book.component
                .tokens
                .iter()
                .map(|token| token.address.clone())
                .collect::<Vec<_>>(),
            vec![bytes(WETH), bytes(USDC)]
        );
        assert!(book.updated_at.is_some());
        let state = book
            .state
            .as_any()
            .downcast_ref::<EuclidState>()
            .expect("a per-pair Euclid state");
        assert_eq!(state.usd_per_quote_token, 1.0);
    }

    /// Books rebuilt from serialized prices match the source's published books for the same
    /// frame.
    #[test]
    fn prices_round_trip_builds_the_published_books() {
        let mut source = test_source(0.0, false);
        let published = source
            .decode(WsPayload::Text(LEVELS_FRAME.into()))
            .unwrap()
            .unwrap();

        let prices = decode_frame(WsPayload::Text(LEVELS_FRAME.into()), 1)
            .unwrap()
            .unwrap();
        let sent = serde_json::to_string(&prices).unwrap();
        let received: EuclidPrices = serde_json::from_str(&sent).unwrap();
        assert_eq!(received, prices);

        assert_eq!(published.books.len(), 1);
        assert_same_books(&source.assembler.books(received), &published.books);
    }

    #[test]
    fn pairs_outside_the_configured_tokens_are_skipped() {
        let unconfigured = "0x1111111111111111111111111111111111111111";
        let books = test_assembler(0.0, false).collect(prices(
            &[pair(WETH, USDC, "3000", "3001", "1"), pair(unconfigured, USDC, "1", "2", "10")]
                .join(","),
        ));
        assert_eq!(books.len(), 1);
    }

    /// Euclid sends its whole book in one frame, so a level it got wrong on one pair costs that
    /// pair and no other.
    #[test]
    fn a_frame_decodes_into_its_valid_pairs() {
        let decoded = prices(
            &[pair(WETH, USDC, "3000", "3001", "1"), pair(WBTC, USDC, "-1", "65000", "1")]
                .join(","),
        );
        assert_eq!(decoded.pairs.len(), 1);
        assert_eq!(decoded.pairs[0].base, bytes(WETH));
    }

    /// A frame with nothing readable in it fails the poll: a published snapshot is complete, so
    /// one carrying no books would withdraw every pair Euclid was serving.
    #[test]
    fn a_frame_with_nothing_valid_in_it_is_an_error() {
        let error =
            decode_frame(text_frame(1, &pair(WETH, USDC, "-1", "3001", "1")), 1).unwrap_err();
        assert!(matches!(error, FeedError::Parsing(_)), "{error:?}");
    }

    #[test]
    fn an_empty_frame_publishes_an_empty_book() {
        // The gateway sends no pairs while its own grid is stale: nothing is priced.
        let books = test_assembler(0.0, false).collect(prices(""));
        assert_eq!(books.len(), 0);
    }

    #[test]
    fn books_below_the_tvl_floor_are_filtered() {
        // (3000 * 0.1 + 3001 * 0.1) / 2 = 300.05 USD
        let thin = prices(&pair(WETH, USDC, "3000", "3001", "0.1"));
        assert_eq!(
            test_assembler(300.0, false)
                .collect(thin.clone())
                .len(),
            1
        );
        assert_eq!(
            test_assembler(301.0, false)
                .collect(thin)
                .len(),
            0
        );
    }

    #[test]
    fn a_quote_token_is_valued_through_the_pair_that_prices_it() {
        // WETH/WBTC is quoted in WBTC, which is worth 60000 USD by its own book.
        let books = test_assembler(0.0, false).collect(prices(
            &[pair(WETH, WBTC, "0.05", "0.051", "1"), pair(WBTC, USDC, "60000", "60100", "1")]
                .join(","),
        ));
        let id = pair_component_id(PROTOCOL_SYSTEM, &bytes(WETH), &bytes(WBTC));
        let state = books[&id.to_string()]
            .state
            .as_any()
            .downcast_ref::<EuclidState>()
            .unwrap()
            .clone();
        assert_eq!(state.usd_per_quote_token, 60000.0);
    }

    #[test]
    fn a_quote_token_nothing_values_in_usd_is_skipped() {
        let books =
            test_assembler(0.0, false).collect(prices(&pair(WETH, WBTC, "0.05", "0.051", "1")));
        assert_eq!(books.len(), 0);
    }

    #[test]
    fn a_frame_for_another_chain_is_rejected() {
        let other_chain = text_frame(8453, &pair(WETH, USDC, "3000", "3001", "1"));
        assert!(matches!(decode_frame(other_chain, 1), Err(FeedError::Parsing(_))));
    }

    /// Prices handed to an assembler for another chain — a wiring mistake between a prices
    /// reader and its assembler — make no books rather than books on the wrong chain.
    #[test]
    fn prices_for_another_chain_assemble_no_books() {
        let prices =
            EuclidPrices { chain_id: 8453, ..prices(&pair(WETH, USDC, "3000", "3001", "1")) };
        assert!(test_assembler(0.0, false)
            .books(prices)
            .is_empty());
    }

    #[test]
    fn every_pair_is_published_under_one_component_for_all_pairs() {
        let books = test_assembler(0.0, true).collect(prices(
            &[pair(WETH, USDC, "3000", "3001", "1"), pair(WBTC, USDC, "60000", "60100", "1")]
                .join(","),
        ));
        assert_eq!(books.len(), 1);
        let book = books.values().next().unwrap();
        assert_eq!(
            QuoteRule::from_attributes(&book.component.static_attributes).unwrap(),
            Some(QuoteRule::OncePerVenue)
        );
        assert!(book
            .state
            .as_any()
            .downcast_ref::<EuclidAllPairsState>()
            .is_some());
    }

    #[test]
    fn decode_skips_binary_frames_and_fails_on_malformed_text() {
        let mut source = test_source(0.0, false);
        assert!(source
            .decode(WsPayload::Binary(vec![1, 2, 3].into()))
            .unwrap()
            .is_none());
        assert!(matches!(
            source.decode(WsPayload::Text("not json".into())),
            Err(FeedError::Parsing(_))
        ));
    }
}
