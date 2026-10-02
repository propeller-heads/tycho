use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{client_builder::HashflowClientBuilder, state::HashflowState};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        constants::get_hashflow_auth,
        models::TimestampHeader,
        protocols::{
            component::{decode_venue, DecodedVenue},
            maker_books::MakerBook,
        },
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for HashflowState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let DecodedVenue { books, tokens, quote_rule } =
            decode_venue::<MakerBook>(&snapshot, all_tokens)?;

        let auth = get_hashflow_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Hashflow authentication: {e}"))
        })?;
        let mut builder = HashflowClientBuilder::new(snapshot.component.chain, auth.user, auth.key)
            .tokens(tokens.keys().cloned().collect());
        if let Some(quote_rule) = quote_rule {
            builder = builder.quote_rule(quote_rule);
        }
        let client = builder.build().map_err(|e| {
            InvalidSnapshotError::MissingAttribute(format!("Couldn't create HashflowClient: {e}"))
        })?;

        HashflowState::new(books, tokens, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::rfq::{
        models::QuoteRule,
        protocols::{
            component::BOOKS_ATTRIBUTE,
            test_utils::{usdc, venue_snapshot, wbtc, weth},
        },
    };

    /// Two makers on WBTC/USDC and one of them on WETH/USDC.
    fn snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");
        let books = serde_json::json!([
            {
                "mm": "test_market_maker",
                "base_token": wbtc().address, "quote_token": usdc().address,
                "levels": [{ "q": "1.5", "p": "65000.0" }, { "q": "2.0", "p": "64950.0" }]
            },
            {
                "mm": "mm_b",
                "base_token": wbtc().address, "quote_token": usdc().address,
                "levels": [{ "q": "0.5", "p": "65100.0" }]
            },
            {
                "mm": "test_market_maker",
                "base_token": weth().address, "quote_token": usdc().address,
                "levels": [{ "q": "10", "p": "3000.0" }]
            }
        ]);
        venue_snapshot("rfq:hashflow", &[wbtc(), usdc(), weth()], &books)
    }

    async fn decode(
        snapshot: ComponentWithState,
        tokens: &HashMap<Bytes, Token>,
    ) -> Result<HashflowState, InvalidSnapshotError> {
        HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1703097600u64 },
            &HashMap::new(),
            tokens,
            &DecoderContext::new(),
        )
        .await
    }

    #[tokio::test]
    async fn test_decodes_books_and_default_rule() {
        let (snapshot, tokens) = snapshot();
        let state = decode(snapshot, &tokens).await.unwrap();

        assert_eq!(state.books.tokens.len(), 3);
        assert_eq!(state.client.quote_rule(), QuoteRule::OncePerMaker);
        assert!(state
            .books
            .used_market_makers
            .is_empty());
        let wbtc_books = state
            .books
            .pair_books(&wbtc().address, &usdc().address);
        assert_eq!(wbtc_books.len(), 2);
        assert_eq!(wbtc_books[0].market_maker, "mm_b");
        assert_eq!(wbtc_books[1].levels[0].quantity, 1.5);
        assert_eq!(wbtc_books[1].levels[0].price, 65000.0);
        assert_eq!(
            state
                .books
                .pair_books(&weth().address, &usdc().address)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_quote_rule_attribute() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"once_per_venue".into());
        let state = decode(snapshot, &tokens).await.unwrap();
        assert_eq!(state.client.quote_rule(), QuoteRule::OncePerVenue);
    }

    #[tokio::test]
    async fn test_missing_books() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .state
            .attributes
            .remove(BOOKS_ATTRIBUTE);
        let state = decode(snapshot, &tokens).await.unwrap();
        assert!(state.books.books.is_empty());
        assert_eq!(state.books.tokens.len(), 3);
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
