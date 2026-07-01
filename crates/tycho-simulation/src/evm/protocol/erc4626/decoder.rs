use std::{collections::HashMap, str::FromStr};

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::ERC4626State;
use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::erc4626::vm,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for ERC4626State {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component =
            crate::evm::protocol::build_swap_quoter_component(&snapshot.component, all_tokens)?;
        let pool_address = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected component id to be pool contract address: {e}"
            ))
        })?;
        let asset_address = snapshot
            .component
            .tokens
            .iter()
            .find(|t| **t != pool_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Failed to determine asset address: no token different from pool address ({pool_address}) was found"
                ))
            })?;
        let asset_token = all_tokens
            .get(asset_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing asset_token in state: {asset_address}"
                ))
            })?;
        let share_token = all_tokens
            .get(&pool_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing share_token in state: {pool_address}"
                ))
            })?;
        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )
        .expect("Failed to create engine");

        let erc4626_state = vm::decode_from_vm(&pool_address, asset_token, share_token, engine)?;
        Ok(erc4626_state.with_component(component))
    }
}
