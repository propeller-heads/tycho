use std::collections::HashMap;

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
        protocols::{
            bebop::client_builder::BebopClientBuilder,
            component::{decode_venue, DecodedVenue},
        },
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for BebopState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let DecodedVenue { books, tokens, quote_rule } =
            decode_venue::<BebopPriceData>(&snapshot, all_tokens)?;
        if quote_rule.is_some_and(|rule| rule != QuoteRule::OncePerVenue) {
            return Err(InvalidSnapshotError::ValueError(
                "Bebop names no market maker; its quote rule is once_per_venue".into(),
            ));
        }

        let auth = get_bebop_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Bebop authentication: {e}"))
        })?;
        let origins = get_bebop_origins().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Bebop origins: {e}"))
        })?;
        let mut builder = BebopClientBuilder::new(snapshot.component.chain, auth.key)
            .tokens(tokens.keys().cloned().collect());
        if let Some(origin_address) = origins.address {
            builder = builder.origin_address(origin_address);
        }
        if let Some(origin_target) = origins.target {
            builder = builder.origin_target(origin_target);
        }
        if let Some(origin_source) = origins.source {
            builder = builder.origin_source(origin_source);
        }
        let client = builder.build().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Couldn't create BebopClient: {e}"))
        })?;

        BebopState::new(books, tokens, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::rfq::protocols::{
        component::BOOKS_ATTRIBUTE,
        test_utils::{usdc, venue_snapshot, wbtc, weth},
    };

    fn snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        env::set_var("BEBOP_KEY", "test_key");
        let books = vec![
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
        ];
        venue_snapshot("rfq:bebop", &[wbtc(), usdc(), weth()], &books)
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
    async fn test_decodes_books() {
        let (snapshot, tokens) = snapshot();
        let state = decode(snapshot, &tokens).await.unwrap();

        assert_eq!(state.tokens.len(), 3);
        assert!(!state.used);
        assert_eq!(state.books.len(), 2);
        assert_eq!(state.books[0].base, wbtc().address.to_vec());
        assert_eq!(state.books[0].get_bids()[0], (65000.0, 1.5));
        assert_eq!(state.books[0].get_asks()[0], (65100.0, 1.0));
        assert_eq!(state.books[1].base, weth().address.to_vec());
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
    async fn test_unknown_quote_rule_attribute() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"twice".into());
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
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
        tokens.remove(&weth().address);
        let result = decode(snapshot, &tokens).await;
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_book_names_token_the_component_lacks() {
        let (mut snapshot, tokens) = snapshot();
        snapshot.component.tokens.pop();
        let result = decode(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("does not carry"))
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
