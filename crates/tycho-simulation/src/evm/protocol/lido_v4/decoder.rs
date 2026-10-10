use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{
    decode_attribute, decode_u32_attribute, LidoV4State, StakingState, BUFFERED_ETHER_ATTR,
    CL_PENDING_BALANCE_ATTR, CL_VALIDATORS_BALANCE_ATTR, DEPOSITED_POST_REPORT_ATTR,
    EXTERNAL_SHARES_ATTR, MAX_STAKE_LIMIT_ATTR, MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR,
    PREV_STAKE_BLOCK_NUMBER_ATTR, PREV_STAKE_LIMIT_ATTR, STETH_COMPONENT_ID, TOTAL_SHARES_ATTR,
    WSTETH_SHARES_ATTR,
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for LidoV4State {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        if !snapshot
            .component
            .id
            .eq_ignore_ascii_case(STETH_COMPONENT_ID)
        {
            return Err(InvalidSnapshotError::ValueError(format!(
                "unknown Lido V4 component id {}",
                snapshot.component.id
            )));
        }

        let raw = |name: &str| -> Result<&Bytes, InvalidSnapshotError> {
            snapshot
                .state
                .attributes
                .get(name)
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
        };
        let value = |name: &str| -> Result<U256, InvalidSnapshotError> {
            decode_attribute(name, raw(name)?).map_err(InvalidSnapshotError::ValueError)
        };
        let value_u32 = |name: &str| -> Result<u32, InvalidSnapshotError> {
            decode_u32_attribute(name, raw(name)?).map_err(InvalidSnapshotError::ValueError)
        };

        let staking_state = StakingState::new(
            value_u32(PREV_STAKE_BLOCK_NUMBER_ATTR)?,
            value(PREV_STAKE_LIMIT_ATTR)?,
            value_u32(MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR)?,
            value(MAX_STAKE_LIMIT_ATTR)?,
        );

        // Seeded from the observed header; `apply_block` moves it to the execution block before
        // the state is quoted.
        Ok(LidoV4State::new(
            block.number,
            value(TOTAL_SHARES_ATTR)?,
            value(EXTERNAL_SHARES_ATTR)?,
            value(BUFFERED_ETHER_ATTR)?,
            value(DEPOSITED_POST_REPORT_ATTR)?,
            value(CL_VALIDATORS_BALANCE_ATTR)?,
            value(CL_PENDING_BALANCE_ATTR)?,
            staking_state,
            value(WSTETH_SHARES_ATTR)?,
        ))
    }
}
