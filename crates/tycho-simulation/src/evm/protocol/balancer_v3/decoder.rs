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
    ///
    /// Token order follows the pool's registration order as reported by the type-specific
    /// immutable-data getter, not `component.tokens` (which Tycho sorts by address): balances,
    /// rates and weights are all indexed in the former.
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

        // Checked before any engine work: components indexed without the attribute can only fail,
        // so they must not cost stateless-contract fetches or getter probing first.
        let vault_bytes = value
            .component
            .static_attributes
            .get(VAULT_ATTRIBUTE)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "balancer_v3 pool {pool_address} carries no `{VAULT_ATTRIBUTE}` static \
                     attribute; it was indexed with an ethereum-balancer-v3 package older than \
                     0.6.0"
                ))
            })?
            .clone();
        let vault = AlloyAddress::try_from(vault_bytes.as_ref()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "balancer_v3 pool {pool_address} carries an invalid `{VAULT_ATTRIBUTE}` static \
                 attribute: {e}"
            ))
        })?;

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

        let pool = AlloyAddress::from_slice(pool_address.as_ref());
        let factory = vm::resolve_pool_type(&value.component.static_attributes, &engine, &pool)
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;
        let state = vm::read_pool_state(
            &engine,
            &pool,
            &vault,
            factory.pool_type,
            &value.component.static_attributes,
            block.timestamp,
        )
        .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;
        // Only the weighted family registers per-token minimum balances. QuantAMM shares
        // `WeightedMath`'s curve but not that check — it bounds swaps by its own trade-size ratio.
        let min_token_balances = match factory.pool_type {
            vm::BalancerPoolType::Weighted => vm::read_weighted_min_token_balances(&engine, &pool),
            vm::BalancerPoolType::Stable |
            vm::BalancerPoolType::Reclamm |
            vm::BalancerPoolType::QuantAmm => Vec::new(),
        };

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
            vault_bytes,
            factory,
            tokens,
            min_token_balances,
            block.timestamp,
            state,
        ))
    }
}
