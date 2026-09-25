use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{models::BebopPriceData, state::BebopState};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        constants::{get_bebop_auth, get_bebop_origins},
        models::{QuoteRule, TimestampHeader},
        protocols::bebop::client_builder::BebopClientBuilder,
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for BebopState {
    type Error = InvalidSnapshotError;

    /// Builds the venue state from the component's `books` attribute, a JSON array of one book
    /// per pair. A missing attribute is a venue with no books. Every token the component carries
    /// must be in `all_tokens`, and every book must name two of them. The `quote_rule` static
    /// attribute sets the reuse rule; absent, the venue quotes once per route.
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

        let books: Vec<BebopPriceData> = match snapshot.state.attributes.get("books") {
            Some(books) => serde_json::from_slice(books).map_err(|e| {
                InvalidSnapshotError::ValueError(format!("Invalid books JSON: {e}"))
            })?,
            None => Vec::new(),
        };
        for book in &books {
            for address in [&book.base, &book.quote] {
                if !tokens.contains_key(&Bytes::from(address.clone())) {
                    return Err(InvalidSnapshotError::ValueError(format!(
                        "Book names token 0x{}, which the component does not carry",
                        hex::encode(address)
                    )));
                }
            }
        }

        let quote_rule = QuoteRule::from_attributes(
            &snapshot.component.static_attributes,
            QuoteRule::OncePerVenue,
        )
        .map_err(InvalidSnapshotError::ValueError)?;

        let auth = get_bebop_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Bebop authentication: {e}"))
        })?;
        let origins = get_bebop_origins().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Bebop origins: {e}"))
        })?;

        let mut client_builder = BebopClientBuilder::new(snapshot.component.chain, auth.key)
            .tokens(
                tokens
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>(),
            )
            .quote_rule(quote_rule);
        if let Some(origin_address) = origins.address {
            client_builder = client_builder.origin_address(origin_address);
        }
        if let Some(origin_target) = origins.target {
            client_builder = client_builder.origin_target(origin_target);
        }
        if let Some(origin_source) = origins.source {
            client_builder = client_builder.origin_source(origin_source);
        }
        let client = client_builder.build().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Couldn't create BebopClient: {e}"))
        })?;

        Ok(BebopState::new(books, tokens, quote_rule, client))
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

    fn usdc() -> Token {
        Token::new(
            &hex::decode("a0b86991c6218a76c1d19d4a2e9eb0ce3606eb48")
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

    fn create_test_books() -> Vec<BebopPriceData> {
        vec![
            BebopPriceData {
                base: wbtc().address.to_vec(),
                quote: usdc().address.to_vec(),
                last_update_ts: 1703097600,
                bids: vec![65000.0, 1.5, 64950.0, 2.0, 64900.0, 0.5],
                asks: vec![65100.0, 1.0, 65150.0, 2.5, 65200.0, 1.5],
            },
            BebopPriceData {
                base: weth().address.to_vec(),
                quote: usdc().address.to_vec(),
                last_update_ts: 1703097600,
                bids: vec![3000.0, 2.0],
                asks: vec![3100.0, 1.5],
            },
        ]
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        env::set_var("BEBOP_KEY", "test_key");
        let tokens: HashMap<Bytes, Token> = [wbtc(), usdc(), weth()]
            .into_iter()
            .map(|token| (token.address.clone(), token))
            .collect();

        let books_json =
            serde_json::to_vec(&create_test_books()).expect("Failed to serialize books");
        let state_attributes = HashMap::from([("books".to_string(), books_json.into())]);

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                attributes: state_attributes,
                component_id: "bebop_wbtc_usdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "bebop_wbtc_usdc".to_string(),
                protocol_system: "bebop".to_string(),
                protocol_type_name: "bebop".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![wbtc().address, usdc().address, weth().address],
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
    ) -> Result<BebopState, InvalidSnapshotError> {
        BebopState::try_from_with_header(
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
        assert_eq!(state.books[0].base, wbtc().address.to_vec());
        assert_eq!(state.books[0].get_bids()[0], (65000.0, 1.5));
        assert_eq!(state.books[0].get_asks()[0], (65100.0, 1.0));
        assert_eq!(state.books[1].base, weth().address.to_vec());
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
    async fn test_try_from_unknown_quote_rule() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"twice".to_vec().into());
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
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
        tokens.remove(&weth().address);
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_try_from_book_names_token_the_component_lacks() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop();
        let result = decode(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("does not carry"))
        );
    }

    #[tokio::test]
    async fn test_try_from_invalid_json() {
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
