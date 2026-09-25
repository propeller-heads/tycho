use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{client_builder::NativeClientBuilder, models::NativePriceData, state::NativeState};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::models::{QuoteRule, TimestampHeader},
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for NativeState {
    type Error = InvalidSnapshotError;

    /// Builds the venue state from the component's `books` attribute, a JSON array of one book
    /// per pair. A missing attribute is a venue with no books. Every token the component carries
    /// must be in `all_tokens`. The `quote_rule` static attribute sets the reuse rule; absent,
    /// the venue quotes once per route.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let mut tokens = HashMap::new();
        for address in &snapshot.component.tokens {
            let token = all_tokens.get(address).ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!("Token not found: {address}"))
            })?;
            tokens.insert(address.clone(), token.clone());
        }

        let books: Vec<NativePriceData> = match snapshot.state.attributes.get("books") {
            Some(books) => serde_json::from_slice(books).map_err(|e| {
                InvalidSnapshotError::ValueError(format!("Invalid books JSON: {e}"))
            })?,
            None => Vec::new(),
        };

        let quote_rule = QuoteRule::from_attributes(
            &snapshot.component.static_attributes,
            QuoteRule::OncePerVenue,
        )
        .map_err(InvalidSnapshotError::ValueError)?;

        let client_builder =
            NativeClientBuilder::from_env(snapshot.component.chain).map_err(|e| {
                InvalidSnapshotError::ValueError(format!(
                    "Failed to get Native Relay authentication: {e}"
                ))
            })?;
        let client = client_builder
            .tokens(
                tokens
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>(),
            )
            .quote_rule(quote_rule)
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!("Couldn't create NativeClient: {e}"))
            })?;

        // `new` validates every book, so a book naming a token the component lacks is refused.
        NativeState::new(books, tokens, quote_rule, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;
    use crate::rfq::protocols::native::models::NativePriceLevel;

    fn weth() -> Token {
        Token::new(
            &hex::decode("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
                .unwrap()
                .into(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &hex::decode("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
                .unwrap()
                .into(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn wbtc() -> Token {
        Token::new(
            &hex::decode("2260fac5e5542a773aa44fbcfedf7c193bc2c599")
                .unwrap()
                .into(),
            "WBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn book(base: &Token, quote: &Token, bid: f64, ask: f64) -> NativePriceData {
        NativePriceData {
            base_address: base.address.clone(),
            quote_address: quote.address.clone(),
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 1.5, price: bid }],
            asks: vec![NativePriceLevel { quantity: 2.0, price: ask }],
        }
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        env::set_var("NATIVE_API_KEY", "test_key");
        let tokens: HashMap<Bytes, Token> = [weth(), usdc(), wbtc()]
            .into_iter()
            .map(|token| (token.address.clone(), token))
            .collect();
        let books =
            vec![book(&weth(), &usdc(), 3000.0, 3010.0), book(&wbtc(), &usdc(), 65000.0, 65100.0)];
        let books_json = serde_json::to_vec(&books).expect("Failed to serialize books");
        let state_attributes = HashMap::from([("books".to_string(), books_json.into())]);

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                attributes: state_attributes,
                component_id: "native_market_1".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "native_market_1".to_string(),
                protocol_system: "rfq:native".to_string(),
                protocol_type_name: "native_relay_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![weth().address, usdc().address, wbtc().address],
                contract_addresses: Vec::new(),
                static_attributes: HashMap::new(),
                change: ChangeType::Creation,
                creation_tx: Bytes::default(),
                created_at: chrono::NaiveDateTime::default(),
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        (snapshot, tokens)
    }

    async fn decode(
        snapshot: ComponentWithState,
        tokens: &HashMap<Bytes, Token>,
    ) -> Result<NativeState, InvalidSnapshotError> {
        NativeState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1703097600u64 },
            &HashMap::new(),
            tokens,
            &DecoderContext::new(),
        )
        .await
    }

    #[tokio::test]
    async fn test_try_from_with_header() {
        let (snapshot, tokens) = create_test_snapshot();
        let state = decode(snapshot, &tokens)
            .await
            .expect("create state from snapshot");

        assert_eq!(state.tokens.len(), 3);
        assert_eq!(state.quote_rule, QuoteRule::OncePerVenue);
        assert!(!state.used);
        assert_eq!(state.books.len(), 2);
        assert_eq!(state.books[0].base_address, weth().address);
        assert_eq!(state.books[0].bids[0].price, 3000.0);
        assert_eq!(state.books[0].bids[0].quantity, 1.5);
        assert_eq!(state.books[0].asks[0].price, 3010.0);
        assert_eq!(state.books[1].base_address, wbtc().address);
    }

    #[tokio::test]
    async fn test_try_from_quote_rule_attribute() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"none".to_vec().into());
        let state = decode(snapshot, &tokens).await.unwrap();
        assert_eq!(state.quote_rule, QuoteRule::None);
    }

    #[tokio::test]
    async fn test_try_from_missing_books() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove("books");
        let state = decode(snapshot, &tokens).await.unwrap();
        assert!(state.books.is_empty());
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        let (snapshot, mut tokens) = create_test_snapshot();
        tokens.remove(&wbtc().address);
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_try_from_book_names_token_the_component_lacks() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop();
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_try_from_invalid_books_json() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.state.attributes.insert(
            "books".to_string(),
            "invalid json"
                .as_bytes()
                .to_vec()
                .into(),
        );
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }
}
