use std::collections::HashMap;

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::uniswap_v2::state::{decode_reserve, UniswapV2State},
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for UniswapV2State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `UniswapV2State`. Errors with a `InvalidSnapshotError`
    /// if either reserve0 or reserve1 attributes are missing.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let reserve = |name: &str| {
            let value = snapshot
                .state
                .attributes
                .get(name)
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))?;
            decode_reserve(name, value).map_err(InvalidSnapshotError::ValueError)
        };
        Ok(Self::new(reserve("reserve0")?, reserve("reserve1")?))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use alloy::primitives::aliases::U112;
    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{models::protocol::ProtocolComponentState, Bytes};

    use super::super::state::UniswapV2State;
    use crate::{
        evm::protocol::test_utils::try_decode_snapshot_with_defaults,
        protocol::errors::InvalidSnapshotError,
    };

    #[tokio::test]
    #[rstest]
    #[case::missing_reserve0("reserve0")]
    #[case::missing_reserve1("reserve1")]
    async fn test_usv2_try_from_missing_attribute(#[case] missing_attribute: &str) {
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

        let result = try_decode_snapshot_with_defaults::<UniswapV2State>(snapshot).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(ref x) if x == missing_attribute
        ));
    }

    fn snapshot_with_reserve0(reserve0: Vec<u8>) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: HashMap::from([
                    ("reserve0".to_string(), Bytes::from(reserve0)),
                    ("reserve1".to_string(), Bytes::from(vec![0])),
                ]),
                balances: HashMap::new(),
            },
            component: Default::default(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[rstest]
    #[case::zero(vec![0x00], "0")]
    #[case::zero_word(vec![0x00; 32], "0")]
    // 2^112 - 1 as to_signed_bytes_be writes it: a zero sign byte, then 14 bytes of 0xff.
    #[case::uint112_max_with_sign_byte(
        [vec![0x00], vec![0xff; 14]].concat(),
        "5192296858534827628530496329220095"
    )]
    #[case::zero_padded_word([vec![0x00; 29], vec![0x01, 0x00, 0x00]].concat(), "65536")]
    #[tokio::test]
    async fn test_usv2_try_from_reads_reserve(#[case] reserve0: Vec<u8>, #[case] expected: &str) {
        let state =
            try_decode_snapshot_with_defaults::<UniswapV2State>(snapshot_with_reserve0(reserve0))
                .await
                .unwrap();

        assert_eq!(state.reserve0, U112::from_str(expected).unwrap());
    }

    #[rstest]
    // 2^112: 0x01 followed by 14 zero bytes.
    #[case::past_uint112([vec![0x01], vec![0x00; 14]].concat())]
    // 2^256: 0x01 followed by 32 zero bytes.
    #[case::past_uint256([vec![0x01], vec![0x00; 32]].concat())]
    #[tokio::test]
    async fn test_usv2_try_from_refuses_reserve_the_pair_cannot_store(#[case] reserve0: Vec<u8>) {
        let snapshot = snapshot_with_reserve0(reserve0);

        let result = try_decode_snapshot_with_defaults::<UniswapV2State>(snapshot).await;

        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }
}
