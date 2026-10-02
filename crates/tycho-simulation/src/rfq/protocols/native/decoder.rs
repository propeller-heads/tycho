use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{client_builder::NativeClientBuilder, models::NativePriceData, state::NativeState};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        models::{QuoteRule, TimestampHeader},
        protocols::component::{decode_venue, DecodedVenue},
    },
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
        let DecodedVenue { books, tokens, quote_rule } =
            decode_venue::<NativePriceData>(&snapshot, all_tokens)?;
        if quote_rule.is_some_and(|rule| rule != QuoteRule::OncePerVenue) {
            return Err(InvalidSnapshotError::ValueError(
                "Native names no market maker; its quote rule is once_per_venue".into(),
            ));
        }

        let client = NativeClientBuilder::from_env(snapshot.component.chain)
            .map_err(|e| {
                InvalidSnapshotError::ValueError(format!(
                    "Failed to get Native Relay authentication: {e}"
                ))
            })?
            .tokens(tokens.keys().cloned().collect())
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!("Couldn't create NativeClient: {e}"))
            })?;

        NativeState::new(books, tokens, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::rfq::protocols::{
        component::BOOKS_ATTRIBUTE,
        native::models::NativePriceLevel,
        test_utils::{usdc, venue_snapshot, wbtc, weth},
    };

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

    fn snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        env::set_var("NATIVE_API_KEY", "test_key");
        let books =
            vec![book(&weth(), &usdc(), 3000.0, 3010.0), book(&wbtc(), &usdc(), 65000.0, 65100.0)];
        venue_snapshot("rfq:native", &[weth(), usdc(), wbtc()], &books)
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
    async fn test_decodes_books() {
        let (snapshot, tokens) = snapshot();
        let state = decode(snapshot, &tokens).await.unwrap();

        assert_eq!(state.tokens.len(), 3);
        assert!(!state.used);
        assert_eq!(state.books.len(), 2);
        assert_eq!(state.books[0].base_address, weth().address);
        assert_eq!(state.books[0].bids[0].price, 3000.0);
        assert_eq!(state.books[0].bids[0].quantity, 1.5);
        assert_eq!(state.books[0].asks[0].price, 3010.0);
        assert_eq!(state.books[1].base_address, wbtc().address);
    }

    #[tokio::test]
    async fn test_once_per_venue_attribute() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"once_per_venue".into());
        assert!(decode(snapshot, &tokens).await.is_ok());
    }

    #[tokio::test]
    async fn test_once_per_maker_attribute() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"once_per_maker".into());
        let result = decode(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("names no market maker"))
        );
    }

    #[tokio::test]
    async fn test_missing_books() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .state
            .attributes
            .remove(BOOKS_ATTRIBUTE);
        let state = decode(snapshot, &tokens).await.unwrap();
        assert!(state.books.is_empty());
    }

    #[tokio::test]
    async fn test_missing_token() {
        let (snapshot, mut tokens) = snapshot();
        tokens.remove(&wbtc().address);
        let result = decode(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("Token not found"))
        );
    }

    #[tokio::test]
    async fn test_book_names_token_the_component_lacks() {
        let (mut snapshot, tokens) = snapshot();
        snapshot.component.tokens.pop();
        let result = decode(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("do not match state tokens"))
        );
    }

    #[tokio::test]
    async fn test_invalid_books_json() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .state
            .attributes
            .insert(BOOKS_ATTRIBUTE.to_string(), b"invalid json".into());
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }
}
