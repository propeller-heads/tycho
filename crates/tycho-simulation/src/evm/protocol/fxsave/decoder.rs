use std::{collections::HashMap, str::FromStr};

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::FxSaveState;
use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::fxsave::vm,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

/// Static attribute naming fxSP, the pool fxSAVE deposits into.
const FX_BASE_POOL: &str = "fx_base_pool";

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FxSaveState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let vault = Bytes::from_str(&snapshot.component.id).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected component id to be the fxSAVE address: {e}"
            ))
        })?;
        let base_pool = snapshot
            .component
            .static_attributes
            .get(FX_BASE_POOL)
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute(FX_BASE_POOL.to_string()))?
            .clone();

        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )?;
        let readings = vm::read_vault(&engine, &vault, &base_pool)?;

        let token = |address: &Bytes| {
            all_tokens
                .get(address)
                .cloned()
                .ok_or_else(|| {
                    InvalidSnapshotError::ValueError(format!(
                        "fxSAVE {vault}: token {address} is missing from the token map"
                    ))
                })
        };
        let vault_token = token(&vault)?;
        let stable_token = token(&readings.stable_token)?;
        let yield_token = token(&readings.yield_token)?;
        Ok(FxSaveState::new(vault_token, base_pool, stable_token, yield_token, &readings)?)
    }
}
