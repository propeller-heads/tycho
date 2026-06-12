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
        constants::get_bebop_auth, models::TimestampHeader,
        protocols::bebop::client_builder::BebopClientBuilder,
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for BebopState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        timestamp_header: TimestampHeader,
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

        let empty_array_bytes: Bytes = "[]".as_bytes().to_vec().into();
        let bids_json = state_attrs
            .get("bids")
            .unwrap_or(&empty_array_bytes);
        let asks_json = state_attrs
            .get("asks")
            .unwrap_or(&empty_array_bytes);

        // Parse bids and asks from JSON
        let bids: Vec<(f32, f32)> = serde_json::from_slice(bids_json)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid bids JSON: {e}")))?;
        let asks: Vec<(f32, f32)> = serde_json::from_slice(asks_json)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid asks JSON: {e}")))?;

        let price_data = BebopPriceData {
            base: base_token.address.to_vec(),
            quote: quote_token.address.to_vec(),
            last_update_ts: timestamp_header.timestamp,
            bids: bids
                .iter()
                .flat_map(|(price, size)| [*price, *size])
                .collect(),
            asks: asks
                .iter()
                .flat_map(|(price, size)| [*price, *size])
                .collect(),
        };

        let auth = get_bebop_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Bebop authentication: {e}"))
        })?;

        let client = BebopClientBuilder::new(snapshot.component.chain, auth.user, auth.key)
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!("Couldn't create BebopClient: {e}"))
            })?;

        Ok(BebopState { base_token, quote_token, price_data, client })
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

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let wbtc_token = wbtc();
        let usdc_token = usdc();

        let mut tokens = HashMap::new();
        tokens.insert(wbtc_token.address.clone(), wbtc_token.clone());
        tokens.insert(usdc_token.address.clone(), usdc_token.clone());

        let mut state_attributes = HashMap::new();
        state_attributes.insert(
            "bids".to_string(),
            "[[65000.0, 1.5], [64950.0, 2.0], [64900.0, 0.5]]"
                .as_bytes()
                .to_vec()
                .into(),
        );
        state_attributes.insert(
            "asks".to_string(),
            "[[65100.0, 1.0], [65150.0, 2.5], [65200.0, 1.5]]"
                .as_bytes()
                .to_vec()
                .into(),
        );

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
                tokens: vec![wbtc_token.address.clone(), usdc_token.address.clone()],
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

    #[tokio::test]
    async fn test_try_from_with_header() {
        env::set_var("BEBOP_USER", "test_user");
        env::set_var("BEBOP_KEY", "test_key");

        let (snapshot, tokens) = create_test_snapshot();

        let result = BebopState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1703097600u64 },
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");

        assert_eq!(result.base_token.symbol, "WBTC");
        assert_eq!(result.quote_token.symbol, "USDC");
        assert_eq!(result.price_data.last_update_ts, 1703097600);
        assert_eq!(result.price_data.get_bids().len(), 3);
        assert_eq!(result.price_data.get_asks().len(), 3);
        assert_eq!(result.price_data.get_bids()[0], (65000.0, 1.5));
        assert_eq!(result.price_data.get_asks()[0], (65100.0, 1.0));
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        env::set_var("BEBOP_USER", "test_user");
        env::set_var("BEBOP_KEY", "test_key");

        // Test missing second token (only one token in array)
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop(); // Remove the second token
        let result = BebopState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_try_from_missing_bids() {
        env::set_var("BEBOP_USER", "test_user");
        env::set_var("BEBOP_KEY", "test_key");

        // Should decode an empty array of bids
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.state.attributes.remove("bids");
        let result = BebopState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");
        assert_eq!(result.price_data.bids.len(), 0);
    }

    #[tokio::test]
    async fn test_try_from_invalid_json() {
        env::set_var("BEBOP_USER", "test_user");
        env::set_var("BEBOP_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();

        // Test invalid bids JSON
        snapshot.state.attributes.insert(
            "bids".to_string(),
            "invalid json"
                .as_bytes()
                .to_vec()
                .into(),
        );
        let result = BebopState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;
        assert!(result.is_err());
    }
}
