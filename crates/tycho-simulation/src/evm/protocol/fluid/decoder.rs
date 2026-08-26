use std::{collections::HashMap, str::FromStr};

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::fluid::{v1::FluidV1, vm},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FluidV1 {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        value: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let pool_address = Bytes::from_str(value.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected component id to be pool contract address: {e}"
            ))
        })?;
        let token0_address = value
            .component
            .tokens
            .first()
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("Missing token0 in component".to_string())
            })?;
        let token0 = all_tokens
            .get(token0_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing token0 in state: {token0_address}"
                ))
            })?;
        let token1_address = value
            .component
            .tokens
            .get(1)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("Missing token1 in component".to_string())
            })?;
        let token1 = all_tokens
            .get(token1_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing token1 in state: {token1_address}"
                ))
            })?;
        let resolver_address = value
            .component
            .static_attributes
            .get("reserves_resolver_address")
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(
                    "Missing reserves resolver address in component".to_string(),
                )
            })?;
        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )
        .expect("Infallible");
        let state = vm::decode_from_vm(&pool_address, token0, token1, resolver_address, &engine)?;

        Ok(state)
    }
}
