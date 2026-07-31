use std::collections::HashMap;

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::{
        cpmm::protocol::cpmm_try_from_with_header, pancakeswap_v2::state::PancakeswapV2State,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for PancakeswapV2State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `PancakeswapV2State`. Errors with a
    /// `InvalidSnapshotError` if either reserve0 or reserve1 attributes are missing.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let (reserve0, reserve1) = cpmm_try_from_with_header(snapshot)?;
        Ok(Self::new(reserve0, reserve1))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{models::protocol::ProtocolComponentState, Bytes};

    use super::super::state::PancakeswapV2State;
    use crate::{
        evm::protocol::test_utils::try_decode_snapshot_with_defaults,
        protocol::errors::InvalidSnapshotError,
    };

    #[tokio::test]
    async fn test_pancakeswap_v2_try_from() {
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: HashMap::from([
                    ("reserve0".to_string(), Bytes::from(vec![0; 32])),
                    ("reserve1".to_string(), Bytes::from(vec![0; 32])),
                ]),
                balances: HashMap::new(),
            },
            component: Default::default(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<PancakeswapV2State>(snapshot).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PancakeswapV2State::new(U256::from(0u64), U256::from(0u64)));
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_reserve0("reserve0")]
    #[case::missing_reserve1("reserve1")]
    async fn test_pancakeswap_v2_try_from_missing_attribute(#[case] missing_attribute: &str) {
        let mut attributes = HashMap::from([
            ("reserve0".to_string(), Bytes::from(vec![0; 32])),
            ("reserve1".to_string(), Bytes::from(vec![0; 32])),
        ]);
        attributes.remove(missing_attribute);

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: Default::default(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<PancakeswapV2State>(snapshot).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(ref x) if x == missing_attribute
        ));
    }
}
