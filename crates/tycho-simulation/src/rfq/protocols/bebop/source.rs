use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use chrono::DateTime;
use http::Request;
use prost::Message as ProstMessage;
use tracing::{debug, warn};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::{pair_component, pair_component_id},
        Book, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    protocol::models::ProtocolComponent,
    rfq::protocols::bebop::{
        client::BebopClient,
        models::{BebopBook, BebopPricingUpdate},
        state::BebopState,
        PROTOCOL_SYSTEM,
    },
    snapshot_feed::{
        errors::FeedError,
        ws::{WsPayload, WsSource},
    },
};

/// Opens Bebop's authenticated pricing WebSocket and decodes every pricing frame into the
/// complete book: each pair between the configured tokens whose normalized TVL clears the
/// threshold, as simulate-ready books.
#[derive(Clone, Debug)]
pub struct BebopBookSource {
    pub book_config: BookFeedConfig,
    // USD-priced tokens the book's TVL is normalized into before the threshold applies.
    pub usd_quote_tokens: Arc<HashSet<Bytes>>,
    // Requests binding quotes; shared with every state this source emits. Its key also
    // authenticates the pricing WebSocket.
    pub client: Arc<BebopClient>,
}

impl BebopBookSource {
    /// Builds the simulation component and state for one streamed pair. The state shares this
    /// source's client, so binding quotes carry the feed's full configuration.
    fn build_book(
        &self,
        component_id: Bytes,
        base_token: Token,
        quote_token: Token,
        book: BebopBook,
    ) -> (ProtocolComponent, BebopState) {
        let component = pair_component(
            component_id,
            PROTOCOL_SYSTEM,
            "bebop_pool",
            self.book_config.chain,
            base_token.clone(),
            quote_token.clone(),
        );
        let state = BebopState { base_token, quote_token, book, client: Arc::clone(&self.client) };
        (component, state)
    }

    /// The book's TVL in USD quote-token units, normalized through a pair that prices its quote
    /// token in one of them when the quote token is not one itself. `None` when no such pair is
    /// in the update.
    fn normalized_tvl(&self, book: &BebopBook, pairs: &[BebopBook]) -> Option<f64> {
        if self
            .usd_quote_tokens
            .contains(&book.quote)
        {
            return Some(book.calculate_tvl(None));
        }
        // Look for a pair containing both our quote token and an approved token.
        // Can be either QUOTE/APPROVED or APPROVED/QUOTE.
        let quote_book = self
            .usd_quote_tokens
            .iter()
            .find_map(|approved_quote_token| {
                pairs.iter().find(|p| {
                    (p.base == book.quote && p.quote == *approved_quote_token) ||
                        (p.quote == book.quote && p.base == *approved_quote_token)
                })
            });
        let Some(quote_book) = quote_book else {
            debug!(
                base = %book.base,
                quote = %book.quote,
                "skipping pair, no book prices its quote token in a USD quote token"
            );
            return None;
        };
        Some(book.calculate_tvl(Some(quote_book)))
    }

    /// Builds the complete book for one pricing update: every pair between requested tokens
    /// whose TVL (normalized into an approved quote token) clears the threshold. Each pair the
    /// update carries becomes its own component in the orientation it was published in, so a pair
    /// Bebop streams in both orientations yields two. Fails when any pair carries an invalid
    /// price level.
    fn build_books(&self, update: BebopPricingUpdate) -> Result<HashMap<String, Book>, FeedError> {
        let pairs = update
            .pairs
            .into_iter()
            .map(BebopBook::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| FeedError::Parsing(format!("Invalid Bebop price level: {e}")))?;
        debug!(pairs = pairs.len(), "decoded pricing update");
        // TVL normalization looks across all pairs, so it runs before the pairs are consumed.
        // Only a pair between configured tokens becomes a book, and only those are normalized —
        // but every pair stays in the list, because a kept pair's quote token may be priced by
        // one that was filtered out.
        let tvls: Vec<Option<f64>> = pairs
            .iter()
            .map(|book| {
                self.book_config
                    .pair_tokens(&book.base, &book.quote)?;
                self.normalized_tvl(book, &pairs)
            })
            .collect();

        let mut books = HashMap::new();
        for (book, tvl) in pairs.into_iter().zip(tvls) {
            let Some((base_token, quote_token)) = self
                .book_config
                .pair_tokens(&book.base, &book.quote)
            else {
                continue;
            };
            let component_id = pair_component_id(PROTOCOL_SYSTEM, &book.base, &book.quote);
            let Some(tvl) = tvl else { continue };
            if !self
                .book_config
                .clears_min_tvl(tvl, &component_id)
            {
                continue;
            }
            // `last_update_ts` is milliseconds on the wire (Bebop's spec says seconds; 13-digit
            // values observed live).
            let updated_at = DateTime::from_timestamp_millis(book.last_update_ts as i64);
            let book_key = component_id.to_string();
            let (component, state) =
                self.build_book(component_id, base_token.clone(), quote_token.clone(), book);
            books.insert(book_key, Book { component, state: Arc::new(state), updated_at });
        }
        Ok(books)
    }
}

impl WsSource for BebopBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    fn request(&self) -> Result<Request<()>, FeedError> {
        self.client
            .pricing_handshake()
            // No retry builds a different request, so the feed ends rather than reconnecting.
            .map_err(|e| FeedError::Fatal(format!("Failed to build the pricing handshake: {e}")))
    }

    fn decode(
        &mut self,
        payload: WsPayload,
    ) -> Result<Option<BookSnapshot<ReceivedAt>>, FeedError> {
        let data = match payload {
            WsPayload::Binary(data) => data,
            // The protobuf stream carries pricing only in binary frames; a text frame is most
            // likely a server-side notice worth seeing in the logs.
            WsPayload::Text(text) => {
                warn!(%text, "unexpected text frame on the pricing stream");
                return Ok(None);
            }
        };
        match BebopPricingUpdate::decode(&data[..]) {
            Ok(update) => self
                .build_books(update)
                .map(|books| Some(BookSnapshot::received_now(books))),
            Err(e) => Err(FeedError::Parsing(format!("Failed to parse protobuf message: {e}"))),
        }
    }
}

#[cfg(test)]
pub fn test_client(pricing_ws_endpoint: String) -> Arc<BebopClient> {
    Arc::new(BebopClient::new(
        "".to_string(),
        pricing_ws_endpoint,
        "test_key".to_string(),
        std::time::Duration::from_secs(5),
        None,
        None,
        None,
    ))
}

/// A WETH/USDC source with USDC as the only USD quote token.
#[cfg(test)]
pub fn test_source(price_ws: String, tvl: f64) -> BebopBookSource {
    let weth: Bytes = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .unwrap();
    let usdc: Bytes = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse()
        .unwrap();
    let tokens = crate::book::test_token_map(&[(&weth, "WETH", 18), (&usdc, "USDC", 6)]);
    BebopBookSource {
        book_config: BookFeedConfig {
            chain: tycho_common::models::Chain::Ethereum,
            tokens: Arc::new(tokens),
            min_tvl_usd: tvl,
        },
        usd_quote_tokens: Arc::new(HashSet::from([usdc])),
        client: test_client(price_ws),
    }
}

#[cfg(test)]
fn weth_usdc_price_data(bid_price: f32) -> crate::rfq::protocols::bebop::models::BebopPriceData {
    use crate::rfq::protocols::bebop::models::BebopPriceData;

    BebopPriceData {
        base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
        last_update_ts: 1752617378,
        bids: vec![bid_price, 0.325717f32],
        asks: vec![bid_price + 0.5f32, 0.325717f32],
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::Chain;

    use super::*;
    use crate::{book::test_token_map, rfq::protocols::bebop::models::BebopPriceData};

    #[rstest::rstest]
    #[case::approved_quote_included(0.0, 3000.0, true)]
    #[case::below_tvl_threshold_excluded(1e12, 3000.0, false)]
    fn build_books_tvl_threshold(
        #[case] tvl_threshold: f64,
        #[case] bid_price: f32,
        #[case] expect_included: bool,
    ) {
        let source = test_source("ws://unused".to_string(), tvl_threshold);

        let update = BebopPricingUpdate { pairs: vec![weth_usdc_price_data(bid_price)] };
        let books = source.build_books(update).unwrap();

        assert_eq!(books.len(), usize::from(expect_included));
        if expect_included {
            let book = books.values().next().unwrap();
            assert_eq!(book.component.protocol_system, "book:bebop");
        }
    }

    #[test]
    fn build_books_skips_unknown_tokens() {
        let source = test_source("ws://unused".to_string(), 0.0);

        let mut price_data = weth_usdc_price_data(3000.0);
        price_data.base = hex::decode("1111111111111111111111111111111111111111").unwrap();
        let update = BebopPricingUpdate { pairs: vec![price_data] };

        assert!(source
            .build_books(update)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn build_books_rejects_an_update_with_an_invalid_level() {
        let source = test_source("ws://unused".to_string(), 0.0);

        let mut price_data = weth_usdc_price_data(3000.0);
        price_data.bids = vec![0.0f32, 1.0f32];
        let update = BebopPricingUpdate { pairs: vec![price_data] };

        assert!(matches!(source.build_books(update), Err(FeedError::Parsing(_))));
    }

    #[test]
    fn build_books_emits_each_published_orientation_as_its_own_component() {
        // Bebop streams some pairs from both sides. Each orientation is its own component,
        // identified by the pair in the order it was published.
        let source = test_source("ws://unused".to_string(), 0.0);
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
        let books = source.build_books(update).unwrap();

        assert_eq!(books.len(), 2);
        let forward = &books[&pair_component_id(PROTOCOL_SYSTEM, &weth, &usdc).to_string()];
        assert_eq!(forward.component.tokens[0].symbol, "WETH");
        assert_eq!(forward.component.tokens[1].symbol, "USDC");
        let reverse = &books[&pair_component_id(PROTOCOL_SYSTEM, &usdc, &weth).to_string()];
        assert_eq!(reverse.component.tokens[0].symbol, "USDC");
        assert_eq!(reverse.component.tokens[1].symbol, "WETH");
    }

    #[test]
    fn build_books_normalizes_unapproved_quote() {
        // WETH/WBTC quotes in WBTC (not approved); a WBTC/USDC pair provides the one-hop
        // price for normalization.
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let tokens = test_token_map(&[(&weth, "WETH", 18), (&wbtc, "WBTC", 8)]);
        let source = BebopBookSource {
            book_config: BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd: 0.0,
            },
            usd_quote_tokens: Arc::new(HashSet::from([usdc])),
            client: test_client("ws://unused".to_string()),
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
        assert_eq!(
            source
                .build_books(update)
                .unwrap()
                .len(),
            1
        );

        // Without it, the unapproved quote cannot be priced and the pair is skipped.
        let update = BebopPricingUpdate { pairs: vec![weth_wbtc] };
        assert!(source
            .build_books(update)
            .unwrap()
            .is_empty());
    }
}
