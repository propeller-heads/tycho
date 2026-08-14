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
        protocol::{
            balancer_v3::{state::BalancerV3State, vm},
            vm::utils::load_stateless_contracts,
        },
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

/// `vault` static attribute emitted by the `ethereum-balancer-v3` Substreams package.
const VAULT_ATTRIBUTE: &str = "vault";

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

        // Both static attributes are resolved before any engine work: components missing them can
        // only fail, so they must not cost stateless-contract fetches first.
        let vault_bytes = value
            .component
            .static_attributes
            .get(VAULT_ATTRIBUTE)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "balancer_v3 pool {pool_address} has no `{VAULT_ATTRIBUTE}`"
                ))
            })?
            .clone();
        let vault = AlloyAddress::try_from(vault_bytes.as_ref()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "balancer_v3 pool {pool_address} carries an invalid `{VAULT_ATTRIBUTE}` static \
                 attribute: {e}"
            ))
        })?;

        let pool = AlloyAddress::from_slice(pool_address.as_ref());
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
        let tokens = value.component.tokens.clone();
        let state = vm::read_pool_state(
            &engine,
            &pool,
            &vault,
            pool_type,
            &tokens,
            &value.component.static_attributes,
            block.timestamp,
        )
        .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;
        // Only the weighted family registers per-token minimum balances. QuantAMM shares
        // `WeightedMath`'s curve but not that check — it bounds swaps by its own trade-size ratio.
        let min_token_balances = match pool_type {
            vm::BalancerPoolType::Weighted => vm::read_weighted_min_token_balances(&engine, &pool),
            vm::BalancerPoolType::Stable |
            vm::BalancerPoolType::Reclamm |
            vm::BalancerPoolType::QuantAmm => Vec::new(),
        };

        Ok(BalancerV3State::new(
            pool_address,
            vault_bytes,
            tokens,
            min_token_balances,
            block.timestamp,
            state,
        ))
    }
}
