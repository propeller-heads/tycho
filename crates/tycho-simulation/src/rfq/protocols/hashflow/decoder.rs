use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    client_builder::HashflowClientBuilder,
    models::{HashflowMarketMakerLevels, HashflowPair, HashflowPriceLevel},
    state::HashflowState,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{constants::get_hashflow_auth, models::TimestampHeader},
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

        // Parse the price levels array from the component attributes.
        // A missing levels key indicates no levels for this market maker.
        let empty_levels_array: Bytes = "[]".as_bytes().to_vec().into();
        let levels_data = state_attrs
            .get("levels")
            .unwrap_or(&empty_levels_array);

        let price_levels: Vec<HashflowPriceLevel> = serde_json::from_slice(levels_data)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid levels JSON: {e}")))?;

        let market_maker = state_attrs
            .get("mm")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("mm attribute not found".to_string())
            })
            .and_then(|mm_bytes| {
                String::from_utf8(mm_bytes.to_vec()).map_err(|_| {
                    InvalidSnapshotError::ValueError("Invalid mm encoding".to_string())
                })
            })?;

        // Create the HashflowMarketMakerLevels using token pair from component
        let levels = HashflowMarketMakerLevels {
            pair: HashflowPair {
                base_token: base_token_address.clone(),
                quote_token: quote_token_address.clone(),
            },
            levels: price_levels,
        };

        // Create HashFlow client with authentication from environment variables
        let auth = get_hashflow_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Hashflow authentication: {e}"))
        })?;

        let client = HashflowClientBuilder::new(snapshot.component.chain, auth.user, auth.key)
            .tokens(HashSet::from([base_token_address.clone(), quote_token_address.clone()]))
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!(
                    "Couldn't create HashflowClient: {e}"
                ))
            })?;

        Ok(HashflowState::new(base_token, quote_token, levels, market_maker, client))
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

    fn create_test_levels() -> serde_json::Value {
        serde_json::json!([
            {
                "q": "1.5",
                "p": "65000.0"
            },
            {
                "q": "2.0",
                "p": "64950.0"
            },
            {
                "q": "0.5",
                "p": "65100.0"
            }
        ])
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let wbtc_token = wbtc();
        let usdc_token = usdc();
        let levels = create_test_levels();

        let mut tokens = HashMap::new();
        tokens.insert(wbtc_token.address.clone(), wbtc_token.clone());
        tokens.insert(usdc_token.address.clone(), usdc_token.clone());

        let mut state_attributes = HashMap::new();

        // Serialize the levels to JSON
        let levels_json = serde_json::to_vec(&levels).expect("Failed to serialize levels");
        state_attributes.insert("levels".to_string(), levels_json.into());

        // Add market maker name
        state_attributes.insert(
            "mm".to_string(),
            "test_market_maker"
                .as_bytes()
                .to_vec()
                .into(),
        );

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                attributes: state_attributes,
                component_id: "hashflow_wbtc_usdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "hashflow_wbtc_usdc".to_string(),
                protocol_system: "hashflow".to_string(),
                protocol_type_name: "hashflow".to_string(),
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
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        let (snapshot, tokens) = create_test_snapshot();

        let result = HashflowState::try_from_with_header(
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
        assert_eq!(result.market_maker, "test_market_maker");
        assert_eq!(result.levels.levels.len(), 3);
        assert_eq!(result.levels.levels[0].quantity, 1.5);
        assert_eq!(result.levels.levels[0].price, 65000.0);
        assert_eq!(result.levels.levels[1].quantity, 2.0);
        assert_eq!(result.levels.levels[1].price, 64950.0);
        assert_eq!(result.levels.levels[2].quantity, 0.5);
        assert_eq!(result.levels.levels[2].price, 65100.0);
    }

    #[tokio::test]
    async fn test_try_from_missing_levels() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove("levels");

        let result = HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state with missing levels should default to empty levels");

        // Should succeed with empty levels
        assert_eq!(result.base_token.symbol, "WBTC");
        assert_eq!(result.quote_token.symbol, "USDC");
        assert_eq!(result.levels.levels.len(), 0);
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        // Test missing second token (only one token in array)
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop(); // Remove the second token

        let result = HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_try_from_too_many_tokens() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        // Test with three tokens instead of two
        let (mut snapshot, mut tokens) = create_test_snapshot();

        let dai_token = Token::new(
            &hex::decode("6b175474e89094c44da98b954eedeac495271d0f")
                .unwrap()
                .into(),
            "DAI",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        tokens.insert(dai_token.address.clone(), dai_token.clone());
        snapshot
            .component
            .tokens
            .push(dai_token.address);

        let result = HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_try_from_invalid_levels_json() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();

        // Insert invalid JSON for levels
        snapshot.state.attributes.insert(
            "levels".to_string(),
            "invalid json"
                .as_bytes()
                .to_vec()
                .into(),
        );

        let result = HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn test_try_from_missing_mm() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.state.attributes.remove("mm");

        let result = HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::MissingAttribute(_)));
    }
}
