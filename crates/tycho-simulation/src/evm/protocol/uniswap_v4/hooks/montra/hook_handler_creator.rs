use super::{decode_paused, hook_handler::MontraHookHandler, MontraPoolConfig, PAUSED_ATTRIBUTE};
use crate::{
    evm::protocol::uniswap_v4::hooks::{
        hook_handler::HookHandler,
        hook_handler_creator::{HookCreationParams, HookHandlerCreator},
    },
    protocol::errors::InvalidSnapshotError,
};

pub struct MontraHookCreator;

impl HookHandlerCreator for MontraHookCreator {
    /// Builds a [`MontraHookHandler`] from the pool's `montra_*` attributes and its `paused`
    /// flag (absent when the pool is not paused, read by [`decode_paused`]), quoting at the
    /// timestamp of the block the snapshot was decoded at.
    ///
    /// Fails with `MissingAttribute` for an absent `montra_*` attribute, with `ValueError` for a
    /// `montra_*` value of the wrong width, and with `ValueError` when the params carry no block
    /// timestamp: the hook prices with `block.timestamp`, and no other time gives the decoded
    /// block's quotes. Terms the hook would refuse decode; quoting refuses them.
    fn instantiate_hook_handler(
        &self,
        params: HookCreationParams<'_>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        let config = MontraPoolConfig::from_attributes(params.attributes)?;
        let paused = params
            .attributes
            .get(PAUSED_ATTRIBUTE)
            .is_some_and(decode_paused);
        let block_timestamp = params
            .block_timestamp()
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(
                    "montra pools need the decoded block's timestamp: the hook's fees depend on it"
                        .to_string(),
                )
            })?;
        Ok(Box::new(MontraHookHandler::new(params.hook_address(), config, paused, block_timestamp)))
    }
}
