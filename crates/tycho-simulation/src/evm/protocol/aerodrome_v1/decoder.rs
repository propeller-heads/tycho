use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::aerodrome_v1::state::AerodromeV1State,
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for AerodromeV1State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into an `AerodromeV1State`. Errors with an
    /// `InvalidSnapshotError` if any required attribute is missing.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let reserve0 = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("reserve0")
                .ok_or(InvalidSnapshotError::MissingAttribute("reserve0".to_string()))?,
        );
        let reserve1 = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("reserve1")
                .ok_or(InvalidSnapshotError::MissingAttribute("reserve1".to_string()))?,
        );
        let stable = snapshot
            .component
            .static_attributes
            .get("is_stable")
            .ok_or(InvalidSnapshotError::MissingAttribute("is_stable".to_string()))?
            .first() ==
            Some(&1);

        let fee = snapshot
            .state
            .attributes
            .get("fee")
            .map(|fee| u32::from(fee.clone()))
            .unwrap_or(0);

        if fee > 10_000 {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Invalid fee value {fee}, expected <= 10000 bps"
            )));
        }

        let token0 = all_tokens
            .get(&snapshot.component.tokens[0])
            .ok_or_else(|| InvalidSnapshotError::ValueError("Token0 not found".to_string()))?;
        let token1 = all_tokens
            .get(&snapshot.component.tokens[1])
            .ok_or_else(|| InvalidSnapshotError::ValueError("Token1 not found".to_string()))?;

        Ok(Self::new(reserve0, reserve1, stable, fee, token0.decimals as u8, token1.decimals as u8))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{
        models::{protocol::ProtocolComponentState, token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    };

    use super::super::state::AerodromeV1State;
    use crate::protocol::{errors::InvalidSnapshotError, models::TryFromWithBlock};

    fn token_keys() -> (Bytes, Bytes) {
        let token0 = Bytes::from([0_u8; 20]);
        let mut addr = [0_u8; 20];
        addr[19] = 1;
        let token1 = Bytes::from(addr);
        (token0, token1)
    }

    fn tokens() -> HashMap<Bytes, Token> {
        let (token0_addr, token1_addr) = token_keys();
        let token0 = Token::new(&token0_addr, "T0", 18, 0, &[Some(10_000)], Chain::Ethereum, 100);
        let token1 = Token::new(&token1_addr, "T1", 6, 0, &[Some(10_000)], Chain::Ethereum, 100);
        HashMap::from([(token0.address.clone(), token0), (token1.address.clone(), token1)])
    }

    #[tokio::test]
    async fn test_aerodrome_v1_try_from() {
        let (token0, token1) = token_keys();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: HashMap::from([
                    ("reserve0".to_string(), Bytes::from(vec![0; 32])),
                    ("reserve1".to_string(), Bytes::from(vec![0; 32])),
                ]),
                balances: HashMap::new(),
            },
            component: tycho_common::models::protocol::ProtocolComponent {
                tokens: vec![token0, token1],
                static_attributes: HashMap::from([("is_stable".to_string(), Bytes::from(vec![0]))]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = AerodromeV1State::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &tokens(),
            &Default::default(),
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            AerodromeV1State::new(U256::from(0u64), U256::from(0u64), false, 0, 18, 6)
        );
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_reserve0("reserve0")]
    #[case::missing_reserve1("reserve1")]
    #[case::missing_is_stable("is_stable")]
    async fn test_aerodrome_v1_try_from_missing_attribute(#[case] missing_attribute: &str) {
        let (token0, token1) = token_keys();
        let mut attributes = HashMap::from([
            ("reserve0".to_string(), Bytes::from(vec![0; 32])),
            ("reserve1".to_string(), Bytes::from(vec![0; 32])),
        ]);
        let mut static_attributes =
            HashMap::from([("is_stable".to_string(), Bytes::from(vec![0]))]);
        match missing_attribute {
            "reserve0" | "reserve1" => {
                attributes.remove(missing_attribute);
            }
            "is_stable" => {
                static_attributes.remove(missing_attribute);
            }
            _ => unreachable!("unexpected attribute under test: {missing_attribute}"),
        }

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: tycho_common::models::protocol::ProtocolComponent {
                tokens: vec![token0, token1],
                static_attributes,
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = AerodromeV1State::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &tokens(),
            &Default::default(),
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(ref x) if x == missing_attribute
        ));
    }

    #[tokio::test]
    async fn test_aerodrome_v1_try_from_invalid_fee() {
        let (token0, token1) = token_keys();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: HashMap::from([
                    ("reserve0".to_string(), Bytes::from(vec![0; 32])),
                    ("reserve1".to_string(), Bytes::from(vec![0; 32])),
                    ("fee".to_string(), Bytes::from(10_001_u32.to_be_bytes().to_vec())),
                ]),
                balances: HashMap::new(),
            },
            component: tycho_common::models::protocol::ProtocolComponent {
                tokens: vec![token0, token1],
                static_attributes: HashMap::from([("is_stable".to_string(), Bytes::from(vec![0]))]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = AerodromeV1State::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &tokens(),
            &Default::default(),
        )
        .await;
        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }

    #[tokio::test]
    async fn test_aerodrome_v1_try_from_zero_fee_indicator() {
        let (token0, token1) = token_keys();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: HashMap::from([
                    ("reserve0".to_string(), Bytes::from(vec![0; 32])),
                    ("reserve1".to_string(), Bytes::from(vec![0; 32])),
                    ("fee".to_string(), Bytes::from(420_u32.to_be_bytes().to_vec())),
                ]),
                balances: HashMap::new(),
            },
            component: tycho_common::models::protocol::ProtocolComponent {
                tokens: vec![token0, token1],
                static_attributes: HashMap::from([("is_stable".to_string(), Bytes::from(vec![1]))]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = AerodromeV1State::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &tokens(),
            &Default::default(),
        )
        .await;

        assert!(result.is_ok());
        let state = result.unwrap();
        assert_eq!(state.fee, 420);
        assert!(state.stable);
        assert_eq!(state.fee(), 0.0);
        assert_eq!(state.decimals0, 18);
        assert_eq!(state.decimals1, 6);
    }
}
