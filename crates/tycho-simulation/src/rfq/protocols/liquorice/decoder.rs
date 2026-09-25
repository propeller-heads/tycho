use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    client_builder::LiquoriceClientBuilder, models::LiquoriceMakerLevels, state::LiquoriceState,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        constants::get_liquorice_auth,
        models::{QuoteRule, TimestampHeader},
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for LiquoriceState {
    type Error = InvalidSnapshotError;

    /// Builds the venue state from the component's `books` attribute, a JSON array of every
    /// market maker's levels per pair. A missing attribute is a venue with no levels. Every
    /// token the component carries must be in `all_tokens`, and every book must name two of
    /// them. The `quote_rule` static attribute sets the reuse rule; absent, every maker quotes
    /// once per route.
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

        let books: Vec<LiquoriceMakerLevels> = match snapshot.state.attributes.get("books") {
            Some(books) => serde_json::from_slice(books).map_err(|e| {
                InvalidSnapshotError::ValueError(format!("Invalid books JSON: {e}"))
            })?,
            None => Vec::new(),
        };
        for book in &books {
            for address in [&book.price.base_token, &book.price.quote_token] {
                if !tokens.contains_key(address) {
                    return Err(InvalidSnapshotError::ValueError(format!(
                        "Book of {} names token {address}, which the component does not carry",
                        book.market_maker
                    )));
                }
            }
        }

        let quote_rule = QuoteRule::from_attributes(
            &snapshot.component.static_attributes,
            QuoteRule::OncePerMaker,
        )
        .map_err(InvalidSnapshotError::ValueError)?;

        let auth = get_liquorice_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Liquorice authentication: {e}"))
        })?;

        let client = LiquoriceClientBuilder::new(snapshot.component.chain, auth.solver, auth.key)
            .tokens(
                tokens
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>(),
            )
            .quote_rule(quote_rule)
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!(
                    "Couldn't create LiquoriceClient: {e}"
                ))
            })?;

        Ok(LiquoriceState::new(books, tokens, quote_rule, client))
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

    /// Two makers on WBTC/USDC and one of them on WETH/USDC.
    fn create_test_books() -> serde_json::Value {
        serde_json::json!([
            {
                "mm": "test_market_maker",
                "price": {
                    "baseToken": wbtc().address.to_string(),
                    "quoteToken": usdc().address.to_string(),
                    "levels": [["65000.0", "1.5"], ["64950.0", "2.0"]],
                    "updatedAt": null
                }
            },
            {
                "mm": "mm_b",
                "price": {
                    "baseToken": wbtc().address.to_string(),
                    "quoteToken": usdc().address.to_string(),
                    "levels": [["65100.0", "0.5"]],
                    "updatedAt": null
                }
            },
            {
                "mm": "test_market_maker",
                "price": {
                    "baseToken": weth().address.to_string(),
                    "quoteToken": usdc().address.to_string(),
                    "levels": [["3000.0", "10"]],
                    "updatedAt": null
                }
            }
        ])
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        env::set_var("LIQUORICE_USER", "test_solver");
        env::set_var("LIQUORICE_KEY", "test_key");
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
                component_id: "liquorice_wbtc_usdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "liquorice_wbtc_usdc".to_string(),
                protocol_system: "liquorice".to_string(),
                protocol_type_name: "liquorice".to_string(),
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
    ) -> Result<LiquoriceState, InvalidSnapshotError> {
        LiquoriceState::try_from_with_header(
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
        assert_eq!(state.quote_rule, QuoteRule::OncePerMaker);
        assert!(state.used_market_makers.is_empty());
        assert_eq!(state.books.len(), 3);
        assert_eq!(state.books[0].market_maker, "test_market_maker");
        assert_eq!(state.books[0].price.base_token, wbtc().address);
        assert_eq!(state.books[0].price.levels.len(), 2);
        assert_eq!(state.books[0].price.levels[0].price, 65000.0);
        assert_eq!(state.books[0].price.levels[0].quantity, 1.5);
        assert_eq!(state.books[1].market_maker, "mm_b");
        assert_eq!(state.books[2].price.base_token, weth().address);
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
