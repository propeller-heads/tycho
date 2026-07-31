use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    client_builder::LiquoriceClientBuilder, models::LiquoriceTokenPairPrice, state::LiquoriceState,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::{constants::get_liquorice_auth, models::TimestampHeader},
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for LiquoriceState {
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

        let empty_prices_map: Bytes = "{}".as_bytes().to_vec().into();
        let prices_data = state_attrs
            .get("prices")
            .unwrap_or(&empty_prices_map);

        let prices_by_mm: HashMap<String, LiquoriceTokenPairPrice> =
            serde_json::from_slice(prices_data).map_err(|e| {
                InvalidSnapshotError::ValueError(format!("Invalid prices JSON: {e}"))
            })?;

        let auth = get_liquorice_auth().map_err(|e| {
            InvalidSnapshotError::ValueError(format!("Failed to get Liquorice authentication: {e}"))
        })?;

        let client = LiquoriceClientBuilder::new(snapshot.component.chain, auth.solver, auth.key)
            .tokens(HashSet::from([base_token_address.clone(), quote_token_address.clone()]))
            .build()
            .map_err(|e| {
                InvalidSnapshotError::MissingAttribute(format!(
                    "Couldn't create LiquoriceClient: {e}"
                ))
            })?;

        Ok(LiquoriceState::new(base_token, quote_token, prices_by_mm, client))
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

    fn create_test_price_levels() -> serde_json::Value {
        serde_json::json!({
            "test_market_maker": {
                "baseToken": "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
                "quoteToken": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "levels": [
                    ["65000.0", "1.5"],
                    ["64950.0", "2.0"],
                    ["65100.0", "0.5"]
                ],
                "updatedAt": null
            }
        })
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let wbtc_token = wbtc();
        let usdc_token = usdc();
        let price_levels = create_test_price_levels();

        let mut tokens = HashMap::new();
        tokens.insert(wbtc_token.address.clone(), wbtc_token.clone());
        tokens.insert(usdc_token.address.clone(), usdc_token.clone());

        let mut state_attributes = HashMap::new();

        let prices_json = serde_json::to_vec(&price_levels).expect("Failed to serialize prices");
        state_attributes.insert("prices".to_string(), prices_json.into());

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                attributes: state_attributes,
                component_id: "liquorice_wbtc_usdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "liquorice_wbtc_usdc".to_string(),
                protocol_system: "liquorice".to_string(),
                protocol_type_name: "liquorice".to_string(),
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
        env::set_var("LIQUORICE_USER", "test_solver");
        env::set_var("LIQUORICE_KEY", "test_key");

        let (snapshot, tokens) = create_test_snapshot();

        let result = LiquoriceState::try_from_with_header(
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
        assert!(result
            .prices_by_mm
            .contains_key("test_market_maker"));
        let mm_price = &result.prices_by_mm["test_market_maker"];
        assert_eq!(mm_price.levels.len(), 3);
        assert_eq!(mm_price.levels[0].quantity, 1.5);
        assert_eq!(mm_price.levels[0].price, 65000.0);
        assert_eq!(mm_price.levels[1].quantity, 2.0);
        assert_eq!(mm_price.levels[1].price, 64950.0);
        assert_eq!(mm_price.levels[2].quantity, 0.5);
        assert_eq!(mm_price.levels[2].price, 65100.0);
    }

    #[tokio::test]
    async fn test_try_from_missing_prices() {
        env::set_var("LIQUORICE_USER", "test_solver");
        env::set_var("LIQUORICE_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove("prices");

        let result = LiquoriceState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state with missing prices should default to empty prices");

        assert_eq!(result.base_token.symbol, "WBTC");
        assert_eq!(result.quote_token.symbol, "USDC");
        assert!(result.prices_by_mm.is_empty());
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        env::set_var("LIQUORICE_USER", "test_solver");
        env::set_var("LIQUORICE_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop();

        let result = LiquoriceState::try_from_with_header(
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
        env::set_var("LIQUORICE_USER", "test_solver");
        env::set_var("LIQUORICE_KEY", "test_key");

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

        let result = LiquoriceState::try_from_with_header(
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
    async fn test_try_from_invalid_prices_json() {
        env::set_var("LIQUORICE_USER", "test_solver");
        env::set_var("LIQUORICE_KEY", "test_key");

        let (mut snapshot, tokens) = create_test_snapshot();

        snapshot.state.attributes.insert(
            "prices".to_string(),
            "invalid json"
                .as_bytes()
                .to_vec()
                .into(),
        );

        let result = LiquoriceState::try_from_with_header(
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
}
