use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{models::EuclidPriceData, state::EuclidState};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{
        constants::get_euclid_config, models::TimestampHeader,
        protocols::euclid::client_builder::EuclidClientBuilder,
    },
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for EuclidState {
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

        let bids: Vec<(f64, f64)> = serde_json::from_slice(bids_json)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid bids JSON: {e}")))?;
        let asks: Vec<(f64, f64)> = serde_json::from_slice(asks_json)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid asks JSON: {e}")))?;

        let price_data = EuclidPriceData {
            base: base_token.address.to_vec(),
            quote: quote_token.address.to_vec(),
            last_update_ts: timestamp_header.timestamp,
            bids,
            asks,
        };

        let config = get_euclid_config();
        let client = EuclidClientBuilder::new(snapshot.component.chain, config.api_key)
            .base_url(config.base_url)
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!("Couldn't create EuclidClient: {e}"))
            })?;

        Ok(EuclidState { base_token, quote_token, price_data, client })
    }
}

#[cfg(test)]
mod tests {
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;

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

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let weth_token = weth();
        let usdc_token = usdc();

        let mut tokens = HashMap::new();
        tokens.insert(weth_token.address.clone(), weth_token.clone());
        tokens.insert(usdc_token.address.clone(), usdc_token.clone());

        let mut state_attributes = HashMap::new();
        state_attributes.insert(
            "bids".to_string(),
            "[[3499.5, 0.5], [3498.0, 1.0]]"
                .as_bytes()
                .to_vec()
                .into(),
        );
        state_attributes.insert(
            "asks".to_string(),
            "[[3501.5, 0.5], [3503.0, 1.0]]"
                .as_bytes()
                .to_vec()
                .into(),
        );

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                attributes: state_attributes,
                component_id: "euclid_weth_usdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "euclid_weth_usdc".to_string(),
                protocol_system: "rfq:euclid".to_string(),
                protocol_type_name: "euclid_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![weth_token.address.clone(), usdc_token.address.clone()],
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
        let (snapshot, tokens) = create_test_snapshot();

        let result = EuclidState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1757500000u64 },
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");

        assert_eq!(result.base_token.symbol, "WETH");
        assert_eq!(result.quote_token.symbol, "USDC");
        assert_eq!(result.price_data.last_update_ts, 1757500000);
        assert_eq!(result.price_data.bids.len(), 2);
        assert_eq!(result.price_data.asks.len(), 2);
        assert_eq!(result.price_data.bids[0], (3499.5, 0.5));
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop();
        let result = EuclidState::try_from_with_header(
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
        // Missing bids attribute decodes as an empty side.
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.state.attributes.remove("bids");
        let result = EuclidState::try_from_with_header(
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
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.state.attributes.insert(
            "bids".to_string(),
            "invalid json"
                .as_bytes()
                .to_vec()
                .into(),
        );
        let result = EuclidState::try_from_with_header(
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
