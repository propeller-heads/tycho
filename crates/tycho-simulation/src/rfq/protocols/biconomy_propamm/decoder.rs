use std::collections::HashMap;

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    client_builder::PropAmmClientBuilder,
    models::{PropAmmLevelsResponse, PropAmmMakerLevels, PropAmmMergedLevel},
    state::PropAmmState,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::models::TimestampHeader,
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for PropAmmState {
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
                "Component must have 2 tokens (tokenIn and tokenOut)".to_string(),
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
        let makers_json = state_attrs
            .get("makers")
            .unwrap_or(&empty_array_bytes);
        let merged_json = state_attrs
            .get("merged")
            .unwrap_or(&empty_array_bytes);

        let makers: Vec<PropAmmMakerLevels> = serde_json::from_slice(makers_json)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid makers JSON: {e}")))?;
        let merged: Vec<PropAmmMergedLevel> = serde_json::from_slice(merged_json)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid merged JSON: {e}")))?;

        let as_of = match state_attrs.get("as_of") {
            Some(bytes) => String::from_utf8(bytes.to_vec())
                .map_err(|_| {
                    InvalidSnapshotError::ValueError("Invalid as_of encoding".to_string())
                })?
                .parse()
                .map_err(|_| {
                    InvalidSnapshotError::ValueError("Invalid as_of integer".to_string())
                })?,
            None => timestamp_header.timestamp,
        };

        let client = PropAmmClientBuilder::new(snapshot.component.chain)
            .pairs(vec![(base_token_address.clone(), quote_token_address.clone())])
            .build()
            .map_err(|e| {
                InvalidSnapshotError::ValueError(format!("Couldn't create PropAmmClient: {e}"))
            })?;

        let levels = PropAmmLevelsResponse {
            chain_id: client.chain_id(),
            token_in: base_token.address.clone(),
            token_out: quote_token.address.clone(),
            merged,
            makers,
            as_of,
        };

        Ok(PropAmmState { base_token, quote_token, levels, client })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;

    fn weth() -> Token {
        Token::new(
            &Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn create_test_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let weth_token = weth();
        let usdc_token = usdc();

        let mut tokens = HashMap::new();
        tokens.insert(weth_token.address.clone(), weth_token.clone());
        tokens.insert(usdc_token.address.clone(), usdc_token.clone());

        let fixture: PropAmmLevelsResponse = serde_json::from_str(
            &std::fs::read_to_string("src/rfq/protocols/biconomy_propamm/test_responses/levels.json")
                .unwrap(),
        )
        .unwrap();

        let mut state_attributes: HashMap<String, Bytes> = HashMap::new();
        state_attributes.insert(
            "makers".to_string(),
            serde_json::to_vec(&fixture.makers)
                .unwrap()
                .into(),
        );
        state_attributes.insert(
            "merged".to_string(),
            serde_json::to_vec(&fixture.merged)
                .unwrap()
                .into(),
        );
        state_attributes.insert("as_of".to_string(), "1784889534".as_bytes().to_vec().into());

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                attributes: state_attributes,
                component_id: "biconomy_propamm_weth_usdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "biconomy_propamm_weth_usdc".to_string(),
                protocol_system: "rfq:biconomy_propamm".to_string(),
                protocol_type_name: "biconomy_propamm_pool".to_string(),
                chain: Chain::Base,
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

        let result = PropAmmState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1784889534u64 },
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");

        assert_eq!(result.base_token.symbol, "WETH");
        assert_eq!(result.quote_token.symbol, "USDC");
        assert_eq!(result.levels.chain_id, 8453);
        assert_eq!(result.levels.as_of, 1784889534);
        assert_eq!(result.levels.makers.len(), 2);
        assert_eq!(result.levels.merged.len(), 3);
        assert_eq!(result.levels.makers[0].levels[0].price, "1878000000");
    }

    #[tokio::test]
    async fn test_try_from_missing_token() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.component.tokens.pop();
        let result = PropAmmState::try_from_with_header(
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
    async fn test_try_from_missing_makers_defaults_to_empty() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove("makers");
        let result = PropAmmState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");
        assert!(result.levels.makers.is_empty());
    }

    #[tokio::test]
    async fn test_try_from_missing_as_of_falls_back_to_header() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove("as_of");
        let result = PropAmmState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 42 },
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("create state from snapshot");
        assert_eq!(result.levels.as_of, 42);
    }

    #[tokio::test]
    async fn test_try_from_invalid_json() {
        let (mut snapshot, tokens) = create_test_snapshot();
        snapshot.state.attributes.insert(
            "makers".to_string(),
            "invalid json"
                .as_bytes()
                .to_vec()
                .into(),
        );
        let result = PropAmmState::try_from_with_header(
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
