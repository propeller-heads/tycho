use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    all_pairs_state::NativeAllPairsState, client_builder::NativeClientBuilder,
    models::NativePriceData,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        models::{ComponentLayout, QuoteRule, TimestampHeader},
        protocols::component::{decode_all_pairs_component, DecodedAllPairs},
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for NativeAllPairsState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let DecodedAllPairs { books, tokens, quote_rule } =
            decode_all_pairs_component::<NativePriceData>(&snapshot, all_tokens)?;
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
            .component_layout(ComponentLayout::AllPairs)
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!("Couldn't create NativeClient: {e}"))
            })?;

        NativeAllPairsState::new(books, tokens, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use num_bigint::BigUint;
    use tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::rfq::protocols::{
        native::models::NativePriceLevel,
        test_utils::{all_pairs_snapshot, decode, usdc, wbtc, weth},
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
        all_pairs_snapshot("rfq:native", &[weth(), usdc(), wbtc()], &books)
    }

    #[tokio::test]
    async fn test_decodes_books() {
        let (snapshot, tokens) = snapshot();
        let state = decode::<NativeAllPairsState>(snapshot, &tokens)
            .await
            .unwrap();

        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            3005.0
        );
        assert_eq!(
            state
                .spot_price(&wbtc(), &usdc())
                .unwrap(),
            65050.0
        );
        let (sell_limit, _) = state
            .get_limits(weth().address, usdc().address)
            .unwrap();
        assert_eq!(sell_limit, BigUint::from(1_500_000_000_000_000_000u64));
    }

    #[tokio::test]
    async fn test_once_per_maker_attribute() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"once_per_maker".into());
        let result = decode::<NativeAllPairsState>(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("names no market maker"))
        );
    }
}
