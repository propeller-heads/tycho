use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{
    decode_attribute, decode_u16_attribute, decode_u64_attribute, BucketAttributes, BucketLimit,
    EtherfiState, PoolState, RedemptionInfo, Venue, WrapperState, BURN_BUCKET, EXIT_FEE_BPS_ATTR,
    EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR, LOW_WATERMARK_BPS_ATTR, MINT_BUCKET, POOL_COMPONENT_ID,
    REDEMPTION_BUCKET, TOTAL_SHARES_ATTR, TOTAL_VALUE_IN_LP_ATTR, TOTAL_VALUE_OUT_OF_LP_ATTR,
    WEETH_SHARES_ATTR, WRAPPER_COMPONENT_ID,
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for EtherfiState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
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
        let value_u64 = |name: &str| -> Result<u64, InvalidSnapshotError> {
            decode_u64_attribute(name, raw(name)?).map_err(InvalidSnapshotError::ValueError)
        };
        let value_u16 = |name: &str| -> Result<u16, InvalidSnapshotError> {
            decode_u16_attribute(name, raw(name)?).map_err(InvalidSnapshotError::ValueError)
        };
        let bucket = |names: &BucketAttributes| -> Result<BucketLimit, InvalidSnapshotError> {
            Ok(BucketLimit {
                capacity: value_u64(names.capacity)?,
                remaining: value_u64(names.remaining)?,
                last_refill: value_u64(names.last_refill)?,
                refill_rate: value_u64(names.refill_rate)?,
            })
        };

        let id = &snapshot.component.id;
        let venue = if id.eq_ignore_ascii_case(POOL_COMPONENT_ID) {
            Venue::Pool(PoolState {
                redemption: RedemptionInfo {
                    limit: bucket(&REDEMPTION_BUCKET)?,
                    exit_fee_split_to_treasury_bps: value_u16(EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR)?,
                    exit_fee_bps: value_u16(EXIT_FEE_BPS_ATTR)?,
                    low_watermark_bps: value_u16(LOW_WATERMARK_BPS_ATTR)?,
                },
                mint_limit: bucket(&MINT_BUCKET)?,
                burn_limit: bucket(&BURN_BUCKET)?,
            })
        } else if id.eq_ignore_ascii_case(WRAPPER_COMPONENT_ID) {
            Venue::Wrapper(WrapperState { weeth_shares: value(WEETH_SHARES_ATTR)? })
        } else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "unknown EtherFi component id {id}"
            )));
        };

        // Seeded from the observed header; `apply_block` moves it to the execution block before
        // the state is quoted.
        Ok(EtherfiState::new(
            block.timestamp,
            value(TOTAL_VALUE_OUT_OF_LP_ATTR)?,
            value(TOTAL_VALUE_IN_LP_ATTR)?,
            value(TOTAL_SHARES_ATTR)?,
            venue,
        ))
    }
}
