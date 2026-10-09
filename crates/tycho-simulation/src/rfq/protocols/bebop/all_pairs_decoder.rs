use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{all_pairs_state::BebopAllPairsState, models::BebopPriceData};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        constants::{get_bebop_auth, get_bebop_origins},
        models::{ComponentLayout, QuoteRule, TimestampHeader},
        protocols::{
            bebop::client_builder::BebopClientBuilder,
            component::{decode_all_pairs_component, DecodedAllPairs},
        },
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for BebopAllPairsState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let DecodedAllPairs { books, tokens, quote_rule } =
            decode_all_pairs_component::<BebopPriceData>(&snapshot, all_tokens)?;
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
            .tokens(tokens.keys().cloned().collect())
            .component_layout(ComponentLayout::AllPairs);
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

        BebopAllPairsState::new(books, tokens, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::rfq::protocols::test_utils::{all_pairs_snapshot, decode, usdc, wbtc, weth};

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
        all_pairs_snapshot("rfq:bebop", &[wbtc(), usdc(), weth()], &books)
    }

    #[tokio::test]
    async fn test_decodes_books() {
        let (snapshot, tokens) = snapshot();
        let state = decode::<BebopAllPairsState>(snapshot, &tokens)
            .await
            .unwrap();

        let wbtc_price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert!((65000.0..=65100.0).contains(&wbtc_price), "{wbtc_price}");
        let weth_price = state
            .spot_price(&weth(), &usdc())
            .unwrap();
        assert!((3000.0..=3100.0).contains(&weth_price), "{weth_price}");
    }

    #[tokio::test]
    async fn test_once_per_maker_attribute() {
        let (mut snapshot, tokens) = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(QuoteRule::ATTRIBUTE.to_string(), b"once_per_maker".into());
        let result = decode::<BebopAllPairsState>(snapshot, &tokens).await;
        assert!(
            matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(msg) if msg.contains("names no market maker"))
        );
    }
}
