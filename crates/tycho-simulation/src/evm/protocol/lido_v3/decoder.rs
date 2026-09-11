use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{
    LidoV3PoolKind, LidoV3State, StakingState, BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR,
    CL_BALANCE_AND_CL_VALIDATORS_ATTR, STAKING_STATE_ATTR, STETH_COMPONENT_ID,
    TOTAL_AND_EXTERNAL_SHARES_ATTR, WSTETH_COMPONENT_ID, WSTETH_SHARES_ATTR,
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for LidoV3State {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let kind = if snapshot
            .component
            .id
            .eq_ignore_ascii_case(STETH_COMPONENT_ID)
        {
            LidoV3PoolKind::StEth
        } else if snapshot
            .component
            .id
            .eq_ignore_ascii_case(WSTETH_COMPONENT_ID)
        {
            LidoV3PoolKind::WstEth
        } else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "unknown Lido V3 component id {}",
                snapshot.component.id
            )));
        };

        let total_and_external_shares = snapshot
            .state
            .attributes
            .get(TOTAL_AND_EXTERNAL_SHARES_ATTR)
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute(TOTAL_AND_EXTERNAL_SHARES_ATTR.to_string())
            })
            .map(|value| U256::from_be_slice(value))?;
        let (total_shares, external_shares) =
            LidoV3State::split_low_high_u128(total_and_external_shares);

        let buffered_and_deposited = snapshot
            .state
            .attributes
            .get(BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR)
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute(
                    BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR.to_string(),
                )
            })
            .map(|value| U256::from_be_slice(value))?;
        let (buffered_ether, deposited_validators) =
            LidoV3State::split_low_high_u128(buffered_and_deposited);

        let cl_balance_and_validators = snapshot
            .state
            .attributes
            .get(CL_BALANCE_AND_CL_VALIDATORS_ATTR)
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute(
                    CL_BALANCE_AND_CL_VALIDATORS_ATTR.to_string(),
                )
            })
            .map(|value| U256::from_be_slice(value))?;
        let (cl_balance, cl_validators) =
            LidoV3State::split_low_high_u128(cl_balance_and_validators);

        let staking_state = match kind {
            LidoV3PoolKind::StEth => Some(StakingState::from_u256(U256::from_be_slice(
                snapshot
                    .state
                    .attributes
                    .get(STAKING_STATE_ATTR)
                    .ok_or_else(|| {
                        InvalidSnapshotError::MissingAttribute(STAKING_STATE_ATTR.to_string())
                    })?,
            ))),
            LidoV3PoolKind::WstEth => None,
        };

        // Only the wstETH component tracks the wrapper's share balance; it bounds unwrapping.
        let wsteth_shares = match kind {
            LidoV3PoolKind::StEth => None,
            LidoV3PoolKind::WstEth => Some(U256::from_be_slice(
                snapshot
                    .state
                    .attributes
                    .get(WSTETH_SHARES_ATTR)
                    .ok_or_else(|| {
                        InvalidSnapshotError::MissingAttribute(WSTETH_SHARES_ATTR.to_string())
                    })?,
            )),
        };

        Ok(LidoV3State::new(
            kind,
            block.number,
            block.timestamp,
            total_shares,
            external_shares,
            buffered_ether,
            deposited_validators,
            cl_balance,
            cl_validators,
            staking_state,
            wsteth_shares,
        ))
    }
}
