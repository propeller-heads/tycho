use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{client_builder::NativeClientBuilder, models::NativePriceData, state::NativeState};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::models::TimestampHeader,
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for NativeState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let state_attrs = snapshot.state.attributes;

        if snapshot.component.tokens.len() != 2 {
            return Err(InvalidSnapshotError::ValueError(
                "Component must have 2 tokens (base and quote)".to_string(),
            ));
        }

        let base_token_address = &snapshot.component.tokens[0];
        let quote_token_address = &snapshot.component.tokens[1];

        let base_token = all_tokens
            .get(base_token_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Base token not found: {base_token_address}"
                ))
            })?
            .clone();

        let quote_token = all_tokens
            .get(quote_token_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Quote token not found: {quote_token_address}"
                ))
            })?
            .clone();

        // Parse the Relay orderbook snapshot stored by the stream.
        let book_data = state_attrs
            .get("book")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("book".to_string()))?;

        let book: NativePriceData = serde_json::from_slice(book_data)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid book JSON: {e}")))?;

        let client_builder =
            NativeClientBuilder::from_env(snapshot.component.chain).map_err(|e| {
                InvalidSnapshotError::ValueError(format!(
                    "Failed to get Native Relay authentication: {e}"
                ))
            })?;

        let client = client_builder
            .tokens(HashSet::from([base_token.address.clone(), quote_token.address.clone()]))
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!("Couldn't create NativeClient: {e}"))
            })?;

        NativeState::new(base_token, quote_token, book, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env};

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

    fn create_test_book() -> NativePriceData {
        NativePriceData {
            base_address: weth().address,
            quote_address: usdc().address,
            minimum_in_base: 0.0,
            minimum_in_quote: 0.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { price: 3000.0, quantity: 1.5 }],
            asks: vec![NativePriceLevel { price: 3001.0, quantity: 2.0 }],
        }
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let weth_token = weth();
        let usdc_token = usdc();
        let book = create_test_book();

        let mut tokens = HashMap::new();
        tokens.insert(weth_token.address.clone(), weth_token.clone());
        tokens.insert(usdc_token.address.clone(), usdc_token.clone());

        let mut state_attributes = HashMap::new();

        let book_json = serde_json::to_vec(&book).expect("Failed to serialize book");
        state_attributes.insert("book".to_string(), book_json.into());

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
                tokens: vec![weth_token.address.clone(), usdc_token.address.clone()],
                contract_addresses: Vec::new(),
                static_attributes: HashMap::new(),
                change: ChangeType::Creation,
                creation_tx: Bytes::default(),
                created_at: chrono::NaiveDateTime::default(),
            },
            component_tvl: Some(4500.0),
            entrypoints: Vec::new(),
        };

        (snapshot, tokens)
    }

    #[tokio::test]
    async fn test_try_from_with_header() {
        env::set_var("NATIVE_API_KEY", "test-api-key");

        let (snapshot, tokens) = create_test_snapshot();

        let result = NativeState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1703097600u64 },
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");

        assert_eq!(result.base_token.symbol, "WETH");
        assert_eq!(result.quote_token.symbol, "USDC");
        assert_eq!(result.book.bids.len(), 1);
        assert_eq!(result.book.asks.len(), 1);
        assert_eq!(result.book.bids[0].price, 3000.0);
        assert_eq!(result.book.bids[0].quantity, 1.5);
    }

    #[tokio::test]
    async fn test_try_from_missing_book() {
        let (mut snapshot, tokens) = create_test_snapshot();
        // Remove the book completely to simulate a missing attribute
        snapshot.state.attributes.remove("book");

        let result = NativeState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(attribute) if attribute == "book"
        ));
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        let (mut snapshot, tokens) = create_test_snapshot();
        // Remove the second token
        snapshot.component.tokens.pop();

        let result = NativeState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }
}
