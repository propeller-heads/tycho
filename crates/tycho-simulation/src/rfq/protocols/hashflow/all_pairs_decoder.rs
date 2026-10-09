use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{all_pairs_state::HashflowAllPairsState, client_builder::HashflowClientBuilder};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        constants::get_hashflow_auth,
        models::{ComponentLayout, TimestampHeader},
        protocols::{
            component::{decode_all_pairs_component, DecodedAllPairs},
            maker_price_levels::MakerPriceLevels,
        },
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for HashflowAllPairsState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let DecodedAllPairs { books: price_levels, tokens, quote_rule } =
            decode_all_pairs_component::<MakerPriceLevels>(&snapshot, all_tokens)?;

        let auth = get_hashflow_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Hashflow authentication: {e}"))
        })?;
        let mut builder = HashflowClientBuilder::new(snapshot.component.chain, auth.user, auth.key)
            .tokens(tokens.keys().cloned().collect())
            .component_layout(ComponentLayout::AllPairs);
        if let Some(quote_rule) = quote_rule {
            builder = builder.quote_rule(quote_rule);
        }
        let client = builder.build().map_err(|e| {
            InvalidSnapshotError::MissingAttribute(format!("Couldn't create HashflowClient: {e}"))
        })?;

        HashflowAllPairsState::new(price_levels, tokens, client)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use num_bigint::BigUint;
    use tycho_common::simulation::{errors::SimulationError, protocol_sim::ProtocolSim};

    use super::*;
    use crate::rfq::{
        models::QuoteRule,
        protocols::test_utils::{all_pairs_snapshot, decode, usdc, wbtc, weth},
    };

    #[tokio::test]
    async fn test_decodes_price_levels() {
        // Two makers on WBTC/USDC and one of them on WETH/USDC.
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");
        let price_levels = serde_json::json!([
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
        let (snapshot, tokens) =
            all_pairs_snapshot("rfq:hashflow", &[wbtc(), usdc(), weth()], &price_levels);
        let state = decode::<HashflowAllPairsState>(snapshot, &tokens)
            .await
            .unwrap();

        let wbtc_price_levels = state
            .price_levels
            .pair_price_levels(&wbtc().address, &usdc().address);
        assert_eq!(wbtc_price_levels.len(), 2);
        assert_eq!(wbtc_price_levels[0].market_maker, "mm_b");
        assert_eq!(wbtc_price_levels[1].levels[0].quantity, 1.5);
        assert_eq!(wbtc_price_levels[1].levels[0].price, 65000.0);
        assert_eq!(
            state
                .price_levels
                .pair_price_levels(&weth().address, &usdc().address)
                .len(),
            1
        );
    }

    /// `mm_b` fills the WBTC swap, and the WETH pair only `test_market_maker` quotes is then
    /// refused — which only `once_per_venue` does.
    #[tokio::test]
    async fn test_decodes_once_per_venue_attribute() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");
        let price_levels = serde_json::json!([
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
        let (mut snapshot, tokens) =
            all_pairs_snapshot("rfq:hashflow", &[wbtc(), usdc(), weth()], &price_levels);
        snapshot
            .component
            .static_attributes
            .insert(
                QuoteRule::ATTRIBUTE.to_string(),
                QuoteRule::OncePerVenue
                    .as_str()
                    .as_bytes()
                    .into(),
            );

        let state = decode::<HashflowAllPairsState>(snapshot, &tokens)
            .await
            .unwrap();
        let after_swap = state
            .get_amount_out(BigUint::from(50_000_000u64), &wbtc(), &usdc())
            .unwrap()
            .new_state;

        let result = after_swap.get_amount_out(
            BigUint::from(1_000_000_000_000_000_000u64),
            &weth(),
            &usdc(),
        );
        assert!(
            matches!(result, Err(SimulationError::RecoverableError(message)) if message.contains("already quoted in this route"))
        );
    }
}
