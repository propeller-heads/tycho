use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};
use tycho_ethereum::BytesCodec;

use super::state::RocketpoolState;
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for RocketpoolState {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `RocketpoolState`. Errors with a
    /// `InvalidSnapshotError` if any required attribute is missing.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let get_u256 = |name: &str| -> Result<U256, InvalidSnapshotError> {
            snapshot
                .state
                .attributes
                .get(name)
                .map(U256::from_bytes)
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
        };

        let get_bool = |name: &str| -> Result<bool, InvalidSnapshotError> {
            snapshot
                .state
                .attributes
                .get(name)
                .map(|val| !U256::from_bytes(val).is_zero())
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
        };

        Ok(RocketpoolState::new(
            get_u256("reth_supply")?,
            get_u256("total_eth")?,
            get_u256("deposit_contract_balance")?,
            get_u256("reth_contract_liquidity")?,
            get_u256("deposit_fee")?,
            get_bool("deposits_enabled")?,
            get_u256("min_deposit_amount")?,
            get_u256("max_deposit_pool_size")?,
            get_bool("deposit_assigning_enabled")?,
            get_u256("deposit_assign_maximum")?,
            get_u256("deposit_assign_socialised_maximum")?,
            get_u256("megapool_queue_requested_total")?,
            get_u256("target_reth_collateral_rate")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{models::protocol::ProtocolComponentState, Bytes};

    use super::super::state::RocketpoolState;
    use crate::{
        evm::protocol::test_utils::try_decode_snapshot_with_defaults,
        protocol::errors::InvalidSnapshotError,
    };

    fn create_test_snapshot() -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "Rocketpool".to_owned(),
                attributes: HashMap::from([
                    (
                        "total_eth".to_string(),
                        Bytes::from(U256::from(100_000_000_000_000_000_000u128).to_be_bytes_vec()),
                    ),
                    (
                        "reth_supply".to_string(),
                        Bytes::from(U256::from(95_000_000_000_000_000_000u128).to_be_bytes_vec()),
                    ),
                    (
                        "deposit_contract_balance".to_string(),
                        Bytes::from(U256::from(50_000_000_000_000_000_000u128).to_be_bytes_vec()),
                    ), // 50 ETH in deposit contract
                    (
                        "reth_contract_liquidity".to_string(),
                        Bytes::from(U256::from(10_000_000_000_000_000_000u128).to_be_bytes_vec()),
                    ), // 10 ETH in rETH contract
                    ("deposits_enabled".to_string(), Bytes::from(vec![0x01])),
                    ("deposit_assigning_enabled".to_string(), Bytes::from(vec![0x01])),
                    (
                        "deposit_fee".to_string(),
                        Bytes::from(U256::from(5_000_000_000_000_000u128).to_be_bytes_vec()),
                    ), // 0.5%
                    (
                        "min_deposit_amount".to_string(),
                        Bytes::from(U256::from(10_000_000_000_000_000u128).to_be_bytes_vec()),
                    ), // 0.01 ETH
                    (
                        "max_deposit_pool_size".to_string(),
                        Bytes::from(
                            U256::from(5_000_000_000_000_000_000_000u128).to_be_bytes_vec(),
                        ),
                    ), // 5000 ETH
                    (
                        "deposit_assign_maximum".to_string(),
                        Bytes::from(U256::from(90u64).to_be_bytes_vec()),
                    ),
                    (
                        "deposit_assign_socialised_maximum".to_string(),
                        Bytes::from(U256::from(0u64).to_be_bytes_vec()),
                    ),
                    (
                        "megapool_queue_requested_total".to_string(),
                        Bytes::from(
                            U256::from(1_000_000_000_000_000_000_000_u128).to_be_bytes_vec(),
                        ),
                    ),
                    (
                        "target_reth_collateral_rate".to_string(),
                        Bytes::from(U256::from(10_000_000_000_000_000_u128).to_be_bytes_vec()),
                    ),
                ]),
                balances: HashMap::new(),
            },
            component: Default::default(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_rocketpool_try_from() {
        let snapshot = create_test_snapshot();
        let result = try_decode_snapshot_with_defaults::<RocketpoolState>(snapshot).await;

        assert!(result.is_ok());
        let state = result.unwrap();
        assert_eq!(state.total_eth, U256::from(100_000_000_000_000_000_000u128));
        assert_eq!(state.reth_supply, U256::from(95_000_000_000_000_000_000u128));
        assert_eq!(state.deposit_contract_balance, U256::from(50_000_000_000_000_000_000u128));
        assert_eq!(state.reth_contract_liquidity, U256::from(10_000_000_000_000_000_000u128));
        assert!(state.deposits_enabled);
        assert!(state.deposit_assigning_enabled);
        assert_eq!(state.deposit_assign_maximum, U256::from(90u64));
        assert_eq!(
            state.megapool_queue_requested_total,
            U256::from(1_000_000_000_000_000_000_000u128)
        );
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_total_eth("total_eth")]
    #[case::missing_reth_supply("reth_supply")]
    #[case::missing_deposit_contract_balance("deposit_contract_balance")]
    #[case::missing_reth_contract_liquidity("reth_contract_liquidity")]
    #[case::missing_deposits_enabled("deposits_enabled")]
    #[case::missing_deposit_assigning_enabled("deposit_assigning_enabled")]
    #[case::missing_deposit_fee("deposit_fee")]
    #[case::missing_min_deposit_amount("min_deposit_amount")]
    #[case::missing_max_deposit_pool_size("max_deposit_pool_size")]
    #[case::missing_deposit_assign_maximum("deposit_assign_maximum")]
    #[case::missing_deposit_assign_socialised_maximum("deposit_assign_socialised_maximum")]
    #[case::missing_megapool_queue_requested_total("megapool_queue_requested_total")]
    #[case::missing_target_reth_collateral_rate("target_reth_collateral_rate")]
    async fn test_rocketpool_try_from_missing_attribute(#[case] missing_attribute: &str) {
        let mut snapshot = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove(missing_attribute);

        let result = try_decode_snapshot_with_defaults::<RocketpoolState>(snapshot).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(ref x) if x == missing_attribute
        ));
    }
}
