//! Decodes a `vm:balancer_v3` snapshot into a native [`BalancerV3State`].
//!
//! Mirrors the Curve hybrid decoder: the VM engine is used to resolve the pool family and read
//! state through the pool's own getters, after which quoting is pure Rust. Pool families the maths
//! library cannot price are rejected here so they never reach the router with wrong numbers.
use std::{collections::HashMap, fmt::Debug, str::FromStr};

use alloy::primitives::Address as AlloyAddress;
use balancer_maths_rust::common::types::PoolState;
use revm::DatabaseRef;
use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, simulation::errors::SimulationError, Bytes};

use crate::{
    evm::{
        engine_db::{create_engine, engine_db_interface::EngineDatabaseInterface, SHARED_TYCHO_DB},
        protocol::{
            balancer_v3::{state::BalancerV3State, vm},
            vm::utils::load_stateless_contracts,
        },
        simulation::SimulationEngine,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, tycho_client::feed::BlockHeader> for BalancerV3State {
    type Error = InvalidSnapshotError;

    /// Decodes a `vm:balancer_v3` snapshot.
    async fn try_from_with_header(
        value: ComponentWithState,
        block: tycho_client::feed::BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let pool_address = Bytes::from_str(value.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "expected balancer_v3 component id to be the pool address: {e}"
            ))
        })?;

        let pool = AlloyAddress::from_slice(pool_address.as_ref());
        // Resolved before any engine work: a component whose family this module cannot quote can
        // only fail, so it must not cost stateless-contract fetches first.
        let pool_type = vm::resolve_pool_type(&value.component.static_attributes, &pool)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;

        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )
        .expect("Infallible");

        // The pool's data getters read through the Vault, which delegatecalls into VaultExtension.
        // That implementation is published as a stateless contract on the component, so its code
        // has to be in the engine before any getter runs.
        load_stateless_contracts(&engine, &value.state.attributes)
            .await
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;

        // The component's token list is the pool's registration order, which its balances, rates
        // and weights are all indexed by.
        BalancerV3State::from_vm(
            &engine,
            pool_address,
            pool_type,
            value.component.tokens.clone(),
            &value.component.static_attributes,
            block.timestamp,
        )
        .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))
    }
}

impl BalancerV3State {
    /// Reads a pool of family `pool_type` through `engine`'s view of the indexed storage.
    pub(super) fn from_vm<D: EngineDatabaseInterface + Clone + Debug>(
        engine: &SimulationEngine<D>,
        pool_address: Bytes,
        pool_type: vm::BalancerPoolType,
        tokens: Vec<Bytes>,
        static_attributes: &HashMap<String, Bytes>,
        block_timestamp: u64,
    ) -> Result<Self, SimulationError>
    where
        <D as DatabaseRef>::Error: Debug,
        <D as EngineDatabaseInterface>::Error: Debug,
    {
        let pool = AlloyAddress::from_slice(pool_address.as_ref());
        let state = vm::read_pool_state(
            engine,
            &pool,
            pool_type,
            &tokens,
            static_attributes,
            block_timestamp,
        )?;
        // Only the weighted family registers per-token minimum balances. QuantAMM shares
        // `WeightedMath`'s curve but not that check — it bounds swaps by its own trade-size ratio.
        let min_token_balances = match pool_type {
            vm::BalancerPoolType::Weighted => vm::read_weighted_min_token_balances(engine, &pool),
            vm::BalancerPoolType::Stable |
            vm::BalancerPoolType::StableSurge |
            vm::BalancerPoolType::Reclamm |
            vm::BalancerPoolType::QuantAmm => Vec::new(),
        };
        let stable_surge = match pool_type {
            vm::BalancerPoolType::StableSurge => {
                let PoolState::Stable(stable) = &state else {
                    return Err(SimulationError::FatalError(format!(
                        "balancer_v3 pool {pool} was read as a non-stable state"
                    )));
                };
                Some(vm::read_stable_surge_hook(engine, &pool, stable.mutable.amp)?)
            }
            vm::BalancerPoolType::Weighted |
            vm::BalancerPoolType::Stable |
            vm::BalancerPoolType::Reclamm |
            vm::BalancerPoolType::QuantAmm => None,
        };
        Ok(Self::new(
            pool_address,
            tokens,
            min_token_balances,
            block_timestamp,
            state,
            stable_surge,
        ))
    }
}
