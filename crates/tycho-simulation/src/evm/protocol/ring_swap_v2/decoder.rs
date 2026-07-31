use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::{
        cpmm::protocol::cpmm_try_from_with_header, ring_swap_v2::state::RingSwapV2State,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for RingSwapV2State {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let (reserve0, reserve1) = cpmm_try_from_with_header(snapshot.clone())?;
        let component_tokens = &snapshot.component.tokens;
        if component_tokens.len() != 2 {
            return Err(InvalidSnapshotError::ValueError(format!(
                "RingSwapV2 component {} has {} tokens, expected 2",
                snapshot.component.id,
                component_tokens.len()
            )));
        }

        let underlying_token0 = static_attribute(&snapshot, "underlying_token0")?;
        let underlying_token1 = static_attribute(&snapshot, "underlying_token1")?;
        let reserves_inverted = static_attribute(&snapshot, "reserves_inverted")?
            .last()
            .copied()
            .unwrap_or_default() ==
            1;

        let (expected_component0, expected_component1) = if reserves_inverted {
            (underlying_token1, underlying_token0)
        } else {
            (underlying_token0, underlying_token1)
        };

        if component_tokens[0] != expected_component0 || component_tokens[1] != expected_component1
        {
            return Err(InvalidSnapshotError::ValueError(format!(
                "RingSwapV2 component {} token order does not match its FewToken metadata",
                snapshot.component.id
            )));
        }

        let backing0 = component_balance(&snapshot, &component_tokens[0])?;
        let backing1 = component_balance(&snapshot, &component_tokens[1])?;

        Ok(RingSwapV2State::new(
            snapshot.component.id,
            reserve0,
            reserve1,
            backing0,
            backing1,
            component_tokens[0].clone(),
            component_tokens[1].clone(),
        ))
    }
}

fn static_attribute(
    snapshot: &ComponentWithState,
    name: &str,
) -> Result<Bytes, InvalidSnapshotError> {
    snapshot
        .component
        .static_attributes
        .get(name)
        .cloned()
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
}

fn component_balance(
    snapshot: &ComponentWithState,
    token: &Bytes,
) -> Result<U256, InvalidSnapshotError> {
    snapshot
        .state
        .balances
        .get(token)
        .map(|balance| U256::from_be_slice(balance))
        .ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!(
                "Missing RingSwapV2 component balance for component {} and token {token:?}",
                snapshot.component.id
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        models::protocol::{ProtocolComponent, ProtocolComponentState},
        Bytes,
    };

    use super::*;
    use crate::protocol::{errors::InvalidSnapshotError, models::TryFromWithBlock};

    fn address(value: u8) -> Bytes {
        Bytes::from(vec![value; 20])
    }

    fn snapshot() -> ComponentWithState {
        let token0 = address(1);
        let token1 = address(2);
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "ring".to_string(),
                attributes: HashMap::from([
                    ("reserve0".to_string(), Bytes::from(vec![10])),
                    ("reserve1".to_string(), Bytes::from(vec![20])),
                ]),
                balances: HashMap::from([
                    (token0.clone(), Bytes::from(vec![7])),
                    (token1.clone(), Bytes::from(vec![8])),
                ]),
            },
            component: ProtocolComponent {
                id: "ring".to_string(),
                tokens: vec![token0.clone(), token1.clone()],
                static_attributes: HashMap::from([
                    ("fw_token0".to_string(), address(3)),
                    ("fw_token1".to_string(), address(4)),
                    ("underlying_token0".to_string(), token0),
                    ("underlying_token1".to_string(), token1),
                    ("reserves_inverted".to_string(), Bytes::from(vec![0])),
                ]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn decodes_component_balances_as_available_backing() {
        let state = RingSwapV2State::try_from_with_header(
            snapshot(),
            BlockHeader::default(),
            &HashMap::new(),
            &HashMap::new(),
            &Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(state.component_id, "ring");
        assert_eq!(state.backing0, U256::from(7));
        assert_eq!(state.backing1, U256::from(8));
    }

    #[tokio::test]
    async fn decodes_inverted_pair_metadata_in_component_order() {
        let mut inverted_snapshot = snapshot();
        inverted_snapshot
            .component
            .static_attributes = HashMap::from([
            ("fw_token0".to_string(), address(3)),
            ("fw_token1".to_string(), address(4)),
            ("underlying_token0".to_string(), address(2)),
            ("underlying_token1".to_string(), address(1)),
            ("reserves_inverted".to_string(), Bytes::from(vec![1])),
        ]);

        let state = RingSwapV2State::try_from_with_header(
            inverted_snapshot,
            BlockHeader::default(),
            &HashMap::new(),
            &HashMap::new(),
            &Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(state.backing0, U256::from(7));
        assert_eq!(state.backing1, U256::from(8));
    }

    #[tokio::test]
    async fn rejects_snapshot_without_component_balance() {
        let mut missing_balance = snapshot();
        missing_balance
            .state
            .balances
            .remove(&address(2));
        let result = RingSwapV2State::try_from_with_header(
            missing_balance,
            BlockHeader::default(),
            &HashMap::new(),
            &HashMap::new(),
            &Default::default(),
        )
        .await;

        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }
}
