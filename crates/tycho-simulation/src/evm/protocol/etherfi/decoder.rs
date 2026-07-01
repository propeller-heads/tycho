use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::etherfi::state::{BucketLimit, EtherfiState, RedemptionInfo, EETH_ADDRESS},
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for EtherfiState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component =
            crate::evm::protocol::build_swap_quoter_component(&snapshot.component, all_tokens)?;

        let total_value_out_of_lp = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("totalValueOutOfLp")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("totalValueOutOfLp".to_string())
                })?,
        );

        let total_value_in_lp = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("totalValueInLp")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("totalValueInLp".to_string())
                })?,
        );

        let total_shares = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("totalShares")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("totalShares".to_string()))?,
        );

        let mut liquidity_pool_native_balance: Option<U256> = None;
        let mut eth_amount_locked_for_withdrawl: Option<U256> = None;
        let mut eth_redemption_info: Option<RedemptionInfo> = None;

        if snapshot.component.id == format!("0x{}", hex::encode(EETH_ADDRESS)) {
            liquidity_pool_native_balance = Some(U256::from_be_slice(
                snapshot
                    .state
                    .attributes
                    .get("liquidityPoolNativeBalance")
                    .ok_or_else(|| {
                        InvalidSnapshotError::MissingAttribute(
                            "liquidityPoolNativeBalance".to_string(),
                        )
                    })?,
            ));
            eth_amount_locked_for_withdrawl = Some(U256::from_be_slice(
                snapshot
                    .state
                    .attributes
                    .get("ethAmountLockedForWithdrawl")
                    .ok_or_else(|| {
                        InvalidSnapshotError::MissingAttribute(
                            "ethAmountLockedForWithdrawl".to_string(),
                        )
                    })?,
            ));

            let eth_bucket_limiter_raw = snapshot
                .state
                .attributes
                .get("ethBucketLimiter")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("ethBucketLimiter".to_string())
                })?;
            let eth_bucket_limiter_value = U256::from_be_slice(eth_bucket_limiter_raw);

            let eth_redemption_info_raw = snapshot
                .state
                .attributes
                .get("ethRedemptionInfo")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("ethRedemptionInfo".to_string())
                })?;
            let eth_redemption_info_value = U256::from_be_slice(eth_redemption_info_raw);

            eth_redemption_info = Some(RedemptionInfo::from_u256(
                BucketLimit::from_u256(eth_bucket_limiter_value),
                eth_redemption_info_value,
            ));
        }

        Ok(EtherfiState::new(
            block.timestamp,
            total_value_out_of_lp,
            total_value_in_lp,
            total_shares,
            eth_amount_locked_for_withdrawl,
            eth_redemption_info,
            liquidity_pool_native_balance,
        )
        .with_component(component))
    }
}
