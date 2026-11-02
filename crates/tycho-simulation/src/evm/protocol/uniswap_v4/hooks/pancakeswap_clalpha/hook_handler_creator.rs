use super::{
    decode_start_timestamp, hook_handler::PancakeswapClAlphaHookHandler, START_TIMESTAMP_ATTRIBUTE,
};
use crate::{
    evm::protocol::uniswap_v4::hooks::{
        hook_handler::HookHandler,
        hook_handler_creator::{HookCreationParams, HookHandlerCreator},
    },
    protocol::errors::InvalidSnapshotError,
};

pub struct PancakeswapClAlphaHookCreator;

impl HookHandlerCreator for PancakeswapClAlphaHookCreator {
    /// Builds a [`PancakeswapClAlphaHookHandler`] from the pool's `alpha_start_timestamp`.
    ///
    /// The gate starts out evaluated at the block the snapshot was decoded at, so a caller that
    /// never calls `apply_block` still sees a pool open once its start time has passed. Without
    /// a block timestamp the pool reads as closed until `apply_block` runs.
    fn instantiate_hook_handler(
        &self,
        params: HookCreationParams<'_>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        let raw = params
            .attributes
            .get(START_TIMESTAMP_ATTRIBUTE)
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute(START_TIMESTAMP_ATTRIBUTE.into())
            })?;
        let start_timestamp = decode_start_timestamp(raw)?;
        Ok(Box::new(PancakeswapClAlphaHookHandler::new(
            params.hook_address(),
            start_timestamp,
            params.block_timestamp().unwrap_or(0),
        )))
    }
}
