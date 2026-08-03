//! Decodes a `vm:balancer_v3` snapshot into a native [`BalancerV3State`].
//!
//! Mirrors the Curve hybrid decoder: the VM engine is used to resolve the pool family and read
//! state through the pool's own getters, after which quoting is pure Rust. Pool families the maths
//! library cannot price are rejected here so they never reach the router with wrong numbers.
use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address as AlloyAddress;
use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::balancer_v3::{state::BalancerV3State, vm},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, tycho_client::feed::BlockHeader> for BalancerV3State {
    type Error = InvalidSnapshotError;

    /// Decodes a `vm:balancer_v3` snapshot.
    ///
    /// Token order follows the pool's registration order as reported by the type-specific
    /// immutable-data getter, not `component.tokens` (which Tycho sorts by address): balances,
    /// rates and weights are all indexed in the former.
    async fn try_from_with_header(
        value: ComponentWithState,
        _block: tycho_client::feed::BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let pool_address = Bytes::from_str(value.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "expected balancer_v3 component id to be the pool address: {e}"
            ))
        })?;

        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )
        .expect("Infallible");

        let pool = AlloyAddress::from_slice(pool_address.as_ref());
        let pool_type = vm::resolve_pool_type(&value.component.static_attributes, &engine, &pool)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;
        let vault = vm::read_vault(&engine, &pool)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;
        let state = vm::read_pool_state(&engine, &pool, &vault, pool_type)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;

        let tokens = state
            .base()
            .tokens
            .iter()
            .map(|token| {
                Bytes::from_str(token).map_err(|e| {
                    InvalidSnapshotError::ValueError(format!(
                        "balancer_v3 pool {pool_address} reported an invalid token {token}: {e}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(BalancerV3State::new(
            pool_address,
            Bytes::from(vault.into_array().to_vec()),
            pool_type,
            tokens,
            state,
        ))
    }
}
