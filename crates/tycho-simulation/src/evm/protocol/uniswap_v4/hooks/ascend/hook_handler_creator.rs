use super::{hook_handler::AscendHookHandler, AscendPoolState};
use crate::{
    evm::protocol::uniswap_v4::hooks::{
        hook_handler::HookHandler,
        hook_handler_creator::{HookCreationParams, HookHandlerCreator},
    },
    protocol::errors::InvalidSnapshotError,
};

pub struct AscendHookCreator;

impl HookHandlerCreator for AscendHookCreator {
    /// Builds an [`AscendHookHandler`] from the pool's `ascend_*` attributes, quoting at the
    /// timestamp of the block the snapshot was decoded at.
    ///
    /// Fails with `MissingAttribute` for an absent `ascend_*` attribute, with `ValueError` for a
    /// value of the wrong width or one the hook could not have stored, and with `ValueError` when
    /// the params carry no block timestamp: the hook prices a sell with `block.timestamp`.
    fn instantiate_hook_handler(
        &self,
        params: HookCreationParams<'_>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        let state = AscendPoolState::from_attributes(params.attributes)?;
        let block_timestamp = params
            .block_timestamp()
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(
                    "ascend pools need the decoded block's timestamp: a sell's fill depends on it"
                        .to_string(),
                )
            })?;
        Ok(Box::new(AscendHookHandler::new(params.hook_address(), state, block_timestamp)))
    }
}
